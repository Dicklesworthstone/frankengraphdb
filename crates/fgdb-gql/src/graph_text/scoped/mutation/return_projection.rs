//! Read projection uses the mutation parser's existing scalar operands and
//! precedence compiler. Only terminal semantics differ: no action or write is
//! constructed. Hidden source columns retain every requested property read.

mod aggregate;
pub(super) mod pipeline;
mod with_aggregate;

use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::set_text::{
    BoundSetTextInput, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::{
    GraphSetColumnType, GraphSetProjection, GraphSetTextError, GraphSetTextErrorKind,
    GraphSetValue, PreparedGraphSet,
};

fn expression_error(source: GraphMutationTextError) -> GraphSetTextError {
    let kind = match source.kind {
        GraphMutationTextErrorKind::Query(kind) => GraphSetTextErrorKind::Pattern(kind),
        GraphMutationTextErrorKind::IntegerExpression(kind) => {
            GraphSetTextErrorKind::IntegerExpression(kind)
        }
        GraphMutationTextErrorKind::IntegerOperand => GraphSetTextErrorKind::IntegerOperand,
        GraphMutationTextErrorKind::IntegerNesting { limit } => {
            GraphSetTextErrorKind::IntegerNesting { limit }
        }
        GraphMutationTextErrorKind::Build(_) => {
            GraphSetTextErrorKind::Expected("read scalar expression")
        }
    };
    GraphSetTextError {
        offset: source.offset,
        kind,
    }
}

/// The column domain of one graph input: a property is a scalar, a bare
/// variable its element, a path function its own result.
fn projection_type(input: &Projection<'_>) -> GraphSetColumnType {
    if input.property.is_some() {
        return GraphSetColumnType::Scalar;
    }
    match input.path {
        None => GraphSetColumnType::Vertex,
        Some(GraphPathFunction::Value) => GraphSetColumnType::Path,
        Some(GraphPathFunction::Length | GraphPathFunction::Type) => GraphSetColumnType::Scalar,
        Some(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
        Some(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
        Some(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
        Some(GraphPathFunction::Labels) => GraphSetColumnType::List,
    }
}

/// Whether a value can statically hold a map, so `.key` may read it
/// (fgdb-2jw3z). A row column qualifies only when its type is dynamic
/// (`schema` is the row scope; graph and resolved scopes pass None, and
/// their columns are graph values or aggregates, never maps).
fn map_capable(
    value: &ReadValueTemplate,
    schema: Option<&[(Name<'_>, GraphSetColumnType)]>,
) -> bool {
    match value {
        ReadValueTemplate::MapLiteral { .. }
        | ReadValueTemplate::MapGet { .. }
        | ReadValueTemplate::Local(_)
        | ReadValueTemplate::Index { .. }
        | ReadValueTemplate::Reduce { .. } => true,
        ReadValueTemplate::Column(at) => schema
            .and_then(|schema| schema.get(*at))
            .is_some_and(|(_, kind)| *kind == GraphSetColumnType::Any),
        _ => false,
    }
}

/// The openCypher list functions parsed as values (fgdb-20foe).
#[derive(Clone, Copy)]
enum ListFunction {
    Head,
    Last,
    Tail,
    Range,
    Reduce,
}

struct GraphProjectionHead<'a> {
    with: bool,
    inputs: Vec<Projection<'a>>,
    outputs: Vec<(Name<'a>, ReadValueTemplate)>,
}
impl<'a> GraphProjectionHead<'a> {
    fn schema(&self, parameters: &[GqlParameterSpec]) -> pipeline::RowSchema<'a> {
        // Match the native source-column domains. A property on an edge is
        // still a scalar; the Edge marker selects its owner, not its value type.
        let types: Vec<_> = self
            .inputs
            .iter()
            .map(|input| {
                if input.property.is_some() {
                    return GraphSetColumnType::Scalar;
                }
                match input.path {
                    Some(GraphPathFunction::Value) => GraphSetColumnType::Path,
                    Some(GraphPathFunction::Length | GraphPathFunction::Type) => {
                        GraphSetColumnType::Scalar
                    }
                    Some(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
                    Some(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
                    Some(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
                    Some(GraphPathFunction::Labels) => GraphSetColumnType::List,
                    None => GraphSetColumnType::Vertex,
                }
            })
            .collect();
        self.outputs
            .iter()
            .map(|(name, operand)| (*name, operand.column_type(&types, parameters)))
            .collect()
    }
}

impl<'a> Parser<'a> {
    /// Parse a MATCH leaf exactly once. Composition owns the enclosing set
    /// delimiters and pagination; this parser owns names, expressions and the
    /// original shared parameter table. No statement substring is rewritten.
    pub(in crate::graph_text) fn parse_return_for_composition(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        if self.is_word("UNWIND")
            || self.is_word("CALL")
            || self.is_word("RETURN")
            || self.is_word("WITH")
        {
            return self.parse_leading_pipeline(statement);
        }
        self.parse_match_prefix()?;
        let mut head = self.graph_projection_head()?;
        self.hoist_boundary_reads(&mut head, 0)?;
        let pipeline = if head.with {
            self.row_pipeline(head.schema(&self.syntax.parameters))?
        } else {
            Vec::new()
        };
        self.end()?;
        self.finish_graph_projection(statement, head, pipeline)
    }

    fn parse_leading_pipeline(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        let mut schema = Vec::new();
        let mut leading = Vec::new();
        // A procedure is a source: it can only start the pipeline.
        if self.is_word("CALL") {
            let at = self.current.at;
            self.advance()?;
            leading.push(self.call_stage(&mut schema, at)?);
        }
        while self.is_word("UNWIND") {
            let at = self.current.at;
            self.advance()?;
            leading.push(self.unwind_stage(&mut schema, at)?);
        }
        let implied = if self.is_word("MATCH") {
            Vec::new()
        } else {
            self.implied_call_vertices(&schema, &leading)?
        };
        if self.is_word("MATCH") || !implied.is_empty() {
            self.read_row_bindings = schema.iter().map(|(name, _)| *name).collect();
            if implied.is_empty() {
                self.parse_match_prefix()?;
            } else {
                // The implied `MATCH (n)` for each read CALL output: exactly
                // the root a written one parses, and nothing else.
                for name in implied {
                    self.capacity(
                        self.syntax.variables.len(),
                        MAX_PATTERN_VERTICES,
                        crate::algebra::PatternLimitDimension::Vertices,
                    )?;
                    self.syntax.variables.push(name);
                }
                self.syntax.root_variables = self.syntax.variables.len();
                self.syntax.return_at = self.current.at;
            }
            let correlations = core::mem::take(&mut self.read_correlations);
            let width = schema.len();
            let rebound = self
                .syntax
                .path
                .into_iter()
                .chain(
                    self.syntax
                        .visible_edges()
                        .filter_map(|(edge, _)| edge.variable),
                )
                .find(|name| {
                    self.read_row_bindings
                        .iter()
                        .any(|row| row.text == name.text)
                });
            if let Some(name) = rebound {
                return Err(GraphSetTextError {
                    offset: name.at,
                    kind: GraphSetTextErrorKind::Expected(
                        "a new name for a path or edge after a leading stage",
                    ),
                });
            }
            let mut inputs = Vec::new();
            let mut bound_correlations = Vec::new();
            // The graph vertices a WITH-less projection exposes after the
            // leading columns, as (name, combined-row column).
            let mut exposed = Vec::new();
            // (leading column, graph vertex input) pairs equal by the join.
            let mut identity = Vec::new();
            for &variable in &self.syntax.variables {
                let index = self.mutation_projection(&mut inputs, variable, None)?;
                // A MATCH vertex reusing a leading column's name IS that
                // value: an identity correlation, never a second binding that
                // shadows the first and silently crosses every row.
                match (self.read_row_bindings.iter()).position(|row| row.text == variable.text) {
                    Some(row) => {
                        bound_correlations.push((row, index));
                        identity.push((row, index));
                    }
                    None => {
                        schema.push((variable, GraphSetColumnType::Vertex));
                        exposed.push((variable, width + index));
                    }
                }
            }
            for (variable, key, row) in correlations {
                let index = self.mutation_projection(&mut inputs, variable, Some(key))?;
                bound_correlations.push((row, index));
            }
            // A CALL output matched as a vertex must be one (fgdb-luq0b):
            // record it, so the host refuses a scalar instead of letting the
            // identity join drop every row.
            if let Some(ReadStageTemplate::Call {
                outputs, vertices, ..
            }) = leading.first_mut()
            {
                let mut rows: Vec<usize> = (identity.iter())
                    .map(|&(row, _)| row)
                    .filter(|&row| row < outputs.len())
                    .collect();
                rows.sort_unstable();
                rows.dedup();
                *vertices = rows;
            }
            // The first WITH is the graph-to-row boundary, exactly as it is
            // for a statement that starts at MATCH: `n.p` there reads a graph
            // property. Later stages see only the projected row.
            let (projection, pipeline) = if self.is_word("WITH") {
                let (outputs, distinct, at) =
                    self.leading_with_head(&schema, width, &mut inputs)?;
                let mut types: Vec<_> = schema[..width].iter().map(|(_, kind)| *kind).collect();
                types.extend(inputs.iter().map(projection_type));
                let next: pipeline::RowSchema<'a> = outputs
                    .iter()
                    .map(|(name, value)| {
                        (*name, value.column_type(&types, &self.syntax.parameters))
                    })
                    .collect();
                let mut pipeline = Vec::new();
                if distinct {
                    // Deduplicate the projected rows, not the graph child's.
                    pipeline.push(ReadStageTemplate::Project {
                        at,
                        projection: next
                            .iter()
                            .enumerate()
                            .map(|(index, (name, _))| ReadProjectionTemplate {
                                name: name.text.to_owned(),
                                value: ReadValueTemplate::Column(index),
                            })
                            .collect(),
                        quantifier: crate::GraphSetQuantifier::Distinct,
                    });
                }
                pipeline.extend(self.row_pipeline(next)?);
                let projection = outputs
                    .into_iter()
                    .map(|(name, value)| ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value,
                    })
                    .collect();
                (projection, pipeline)
            } else {
                // Without a WITH the row is the leading columns, then the new
                // MATCH vertices. A leading column a MATCH vertex correlates
                // with is carried as that vertex's graph column (equal by the
                // join, and typed Vertex), so a later `n.p` reads its
                // properties through hidden boundary reads.
                let outputs = (self.read_row_bindings.iter().copied().enumerate())
                    .map(|(row, name)| {
                        let column = identity
                            .iter()
                            .find(|&&(at, _)| at == row)
                            .map_or(row, |&(_, input)| width + input);
                        (name, ReadValueTemplate::Column(column))
                    })
                    .chain(
                        (exposed.into_iter())
                            .map(|(name, column)| (name, ReadValueTemplate::Column(column))),
                    )
                    .collect();
                let mut head = GraphProjectionHead {
                    with: true,
                    inputs: core::mem::take(&mut inputs),
                    outputs,
                };
                self.hoist_boundary_reads(&mut head, width)?;
                inputs = head.inputs;
                let mut types: Vec<_> = schema[..width].iter().map(|(_, kind)| *kind).collect();
                types.extend(inputs.iter().map(projection_type));
                let next: pipeline::RowSchema<'a> = (head.outputs.iter())
                    .map(|(name, value)| {
                        (*name, value.column_type(&types, &self.syntax.parameters))
                    })
                    .collect();
                let projection = (head.outputs.into_iter())
                    .map(|(name, value)| ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value,
                    })
                    .collect();
                (projection, self.row_pipeline(next)?)
            };
            self.end()?;
            self.syntax.columns = inputs
                .into_iter()
                .map(|source| Column {
                    variable: source.variable,
                    property: source.property,
                    path: source.path,
                    alias: source.variable,
                })
                .collect();
            let projection = Some(projection);
            return Ok(UnresolvedGraphText {
                statement,
                syntax: self.syntax,
                projection,
                pipeline,
                singleton: false,
                leading,
                leading_types: vec![GraphSetColumnType::Any; width],
                correlations: bound_correlations,
            });
        }
        let pipeline = self.row_pipeline(schema)?;
        self.end()?;
        Ok(UnresolvedGraphText {
            statement,
            syntax: self.syntax,
            projection: None,
            pipeline: leading.into_iter().chain(pipeline).collect(),
            singleton: true,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        })
    }

    /// Same public binding order as native RETURN *: named vertices, a
    /// captured path, then named edges. Anonymous compiler slots stay private.
    fn visible_graph_bindings(&self) -> impl Iterator<Item = Name<'a>> + '_ {
        self.syntax
            .variables
            .iter()
            .copied()
            .filter(|name| !name.text.starts_with(Self::ANONYMOUS_PREFIX))
            .chain(self.syntax.path)
            .chain(
                self.syntax
                    .visible_edges()
                    .filter_map(|(edge, _)| edge.variable),
            )
    }

    /// The first WITH after `UNWIND ... MATCH`: each item may read a leading
    /// UNWIND column by name, or a MATCH variable or `variable.property`,
    /// which becomes a hidden graph input column (the same
    /// `mutation_projection` the correlations and the MATCH-first head use).
    /// Column indices address the combined row: leading columns, then graph
    /// inputs. Returns the named items, whether DISTINCT was requested, and
    /// the WITH offset.
    #[allow(clippy::type_complexity)]
    fn leading_with_head(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        width: usize,
        inputs: &mut Vec<Projection<'a>>,
    ) -> Result<(Vec<(Name<'a>, ReadValueTemplate)>, bool, usize), GraphSetTextError> {
        if self.with_has_aggregate()? {
            let at = self.current.at;
            let head = self.with_graph_head(&schema[..width], core::mem::take(inputs), false)?;
            *inputs = head.inputs;
            return Ok((head.outputs, false, at));
        }
        let at = self.current.at;
        self.word("WITH")?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_all_quantifier()?;
        }
        let leading: Vec<Name<'a>> = schema[..width].iter().map(|(name, _)| *name).collect();
        let mut outputs = Vec::<(Name<'a>, ReadValueTemplate)>::new();
        if self.take(b'*')? {
            // Leading columns, then the same visible graph bindings as a
            // MATCH-first `WITH *`; anonymous compiler slots stay private.
            for (index, name) in leading.iter().enumerate() {
                outputs.push((*name, ReadValueTemplate::Column(index)));
            }
            let visible: Vec<_> = self.visible_graph_bindings().collect();
            for variable in visible {
                // A vertex reusing a leading name is that leading column.
                if leading.iter().any(|name| name.text == variable.text) {
                    continue;
                }
                let index = self.mutation_projection(inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(width + index)));
            }
            return Ok((outputs, distinct, at));
        }
        loop {
            self.capacity(
                outputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let item_at = self.current.at;
            let value = self.read_resolved_value(
                &mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else {
                        return Ok(None);
                    };
                    if matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
                        return Ok(None);
                    }
                    let graph = parser.syntax.variables.iter().any(|v| v.text == word)
                        || parser.syntax.path.is_some_and(|path| path.text == word)
                        || parser.syntax.visible_edge(word).is_some();
                    if graph {
                        let variable = parser.any_variable()?;
                        let property = if parser.take(b'.')? {
                            Some(parser.name()?)
                        } else {
                            None
                        };
                        let index = parser.mutation_projection(inputs, variable, property)?;
                        return Ok(Some(width + index));
                    }
                    if let Some(index) = leading.iter().position(|name| name.text == word) {
                        parser.advance()?;
                        return Ok(Some(index));
                    }
                    Ok(None)
                },
                0,
            )?;
            let alias = if self.take_word("AS")? {
                self.name()?
            } else if let ReadValueTemplate::Column(index) = &value {
                if *index < width {
                    leading[*index]
                } else {
                    let input = inputs[*index - width];
                    input.property.unwrap_or(input.variable)
                }
            } else {
                return Err(GraphSetTextError {
                    offset: item_at,
                    kind: GraphSetTextErrorKind::Expected("AS alias for a computed WITH value"),
                });
            };
            if outputs.iter().any(|(name, _)| name.text == alias.text) {
                return Err(error(
                    alias.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                )
                .into());
            }
            outputs.push((alias, value));
            if !self.take(b',')? {
                break;
            }
        }
        Ok((outputs, distinct, at))
    }

    /// Shared graph-to-row boundary. Exact grouped RETURN uses this same first
    /// WITH projection and row-stage parser, not a synthetic RETURN statement.
    fn graph_projection_head(&mut self) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        if self.with_has_aggregate()? {
            return self.with_graph_head(&[], Vec::new(), false);
        }
        if self.is_word("UNWIND") {
            let mut inputs = Vec::new();
            let mut outputs = Vec::new();
            for variable in self.visible_graph_bindings() {
                let index = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(index)));
            }
            return Ok(GraphProjectionHead {
                with: true,
                inputs,
                outputs,
            });
        }
        let with = self.take_word("WITH")?;
        if !with {
            self.word("RETURN")?;
        }
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct {
            self.take_all_quantifier()?;
        }
        let mut inputs = Vec::<Projection<'a>>::new();
        let mut outputs = Vec::<(Name<'a>, ReadValueTemplate)>::new();
        if self.take(b'*')? {
            for variable in self.visible_graph_bindings() {
                let at = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(at)));
            }
        } else {
            loop {
                self.capacity(
                    outputs.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = self.current.at;
                let operand = self.read_graph_value(&mut inputs, 0)?;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if let ReadValueTemplate::Column(index) = &operand {
                    inputs[*index].property.unwrap_or(inputs[*index].variable)
                } else {
                    return Err(GraphSetTextError {
                        offset: at,
                        kind: GraphSetTextErrorKind::Expected(
                            "AS alias for a computed RETURN value",
                        ),
                    });
                };
                if outputs.iter().any(|(name, _)| name.text == alias.text) {
                    return Err(error(
                        alias.at,
                        GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                    )
                    .into());
                }
                outputs.push((alias, operand));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok(GraphProjectionHead {
            with,
            inputs,
            outputs,
        })
    }

    fn finish_graph_projection(
        mut self,
        statement: &'a str,
        head: GraphProjectionHead<'a>,
        pipeline: Vec<ReadStageTemplate>,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        let GraphProjectionHead {
            with,
            mut inputs,
            outputs,
        } = head;
        if !with
            && outputs
                .iter()
                .all(|(_, operand)| matches!(operand, ReadValueTemplate::Column(_)))
        {
            // Keep the existing plan/counters/transcript for plain projections,
            // including repeated fields under different public aliases.
            self.syntax.columns = outputs
                .into_iter()
                .map(|(alias, operand)| {
                    let ReadValueTemplate::Column(index) = operand else {
                        unreachable!("plain projection checked above")
                    };
                    let source = inputs[index];
                    Column {
                        variable: source.variable,
                        property: source.property,
                        path: source.path,
                        alias,
                    }
                })
                .collect();
            return Ok(UnresolvedGraphText {
                statement,
                syntax: self.syntax,
                projection: None,
                pipeline,
                singleton: false,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
            });
        }
        if inputs.is_empty() {
            let variable = self.syntax.variables[0];
            let _ = self.mutation_projection(&mut inputs, variable, None)?;
        }
        self.syntax.columns = inputs
            .into_iter()
            .map(|source| Column {
                variable: source.variable,
                property: source.property,
                path: source.path,
                alias: source.variable,
            })
            .collect();
        let mut projection = Vec::new();
        for (alias, value) in outputs {
            projection.push(ReadProjectionTemplate {
                name: alias.text.to_owned(),
                value,
            });
        }
        Ok(UnresolvedGraphText {
            statement,
            syntax: self.syntax,
            projection: Some(projection),
            pipeline,
            singleton: false,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        })
    }

    fn read_value_template(
        &self,
        operand: Operand,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphPatternTextError> {
        Ok(match operand {
            Operand::Column(input) => ReadValueTemplate::Column(input),
            Operand::Literal(value) => ReadValueTemplate::Literal(value),
            Operand::Number(Number::Literal(value)) => {
                ReadValueTemplate::Literal(scalar(value, at)?)
            }
            Operand::Number(Number::Parameter(index)) => ReadValueTemplate::Parameter {
                index,
                at: self.syntax.parameter_offsets[index],
            },
            Operand::Integer { program, at } => ReadValueTemplate::Integer { program, at },
        })
    }

    /// openCypher map projection over a graph row (fgdb-20foe): `n{.a}` reads
    /// n's property a, `k: e` is any value, and `v` is the binding v itself.
    /// Keys are sorted and must be unique, as in a map literal. The source is
    /// the map's guard: a NULL n (an OPTIONAL MATCH without a witness) makes
    /// the whole projection NULL, never `{a: NULL}`. `.*` needs the complete
    /// property catalog and refuses.
    #[allow(clippy::type_complexity)]
    fn map_projection(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<
            &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        >,
        depth: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let source = self.any_variable()?;
        self.punct(b'{', "{")?;
        let mut entries: Vec<(Box<str>, ReadValueTemplate)> = Vec::new();
        if !self.take(b'}')? {
            loop {
                self.capacity(
                    entries.len(),
                    crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let entry = if self.take(b'.')? {
                    if self.is_punct(b'*') {
                        return Err(GraphSetTextError {
                            offset: self.current.at,
                            kind: GraphSetTextErrorKind::Expected(
                                "explicit property keys in a map projection (.* needs the property catalog)",
                            ),
                        });
                    }
                    let key = self.name()?;
                    let column = self.mutation_projection(columns, source, Some(key))?;
                    (key.text.into(), ReadValueTemplate::Column(column))
                } else {
                    let key = self.name()?;
                    if self.take(b':')? {
                        let value = self.read_recursive_value(
                            Some(&mut *columns),
                            schema,
                            resolve,
                            depth + 1,
                        )?;
                        (key.text.into(), value)
                    } else if self
                        .syntax
                        .variables
                        .iter()
                        .any(|name| name.text == key.text)
                        || self.syntax.visible_edge(key.text).is_some()
                    {
                        let column = self.mutation_projection(columns, key, None)?;
                        (key.text.into(), ReadValueTemplate::Column(column))
                    } else {
                        return Err(
                            error(key.at, GraphPatternTextErrorKind::UnknownVariable).into()
                        );
                    }
                };
                entries.push(entry);
                if self.take(b'}')? {
                    break;
                }
                self.punct(b',', ", or }")?;
            }
        }
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::Expected("unique keys in a map projection"),
            });
        }
        let guard = self.mutation_projection(columns, source, None)?;
        let (keys, values): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        Ok(ReadValueTemplate::MapLiteral {
            keys: keys.into_boxed_slice(),
            values,
            guard: Some(Box::new(ReadValueTemplate::Column(guard))),
        })
    }

    fn read_graph_value(
        &mut self,
        inputs: &mut Vec<Projection<'a>>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(Some(inputs), &[], &mut None, depth)
    }

    pub(in crate::graph_text) fn read_row_value(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, schema, &mut None, depth)
    }

    /// Resolve grouped keys and aggregate calls as column leaves while using
    /// the same scalar precedence, parameters and list grammar as ordinary rows.
    pub(in crate::graph_text) fn read_resolved_value(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, &[], &mut Some(resolve), depth)
    }

    #[allow(clippy::type_complexity)]
    fn read_recursive_value(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<
            &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        >,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let at = self.current.at;
        if depth > 64 {
            return Err(GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::IntegerNesting { limit: 64 },
            });
        }
        let comprehension = self.is_punct(b'[') && {
            let mut lexer = self.lexer.clone();
            matches!(lexer.next()?.kind, TokenKind::Word(_))
                && matches!(lexer.next()?.kind,
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("IN"))
        };
        let mut value = if comprehension {
            // openCypher `[x IN list WHERE p | e]` (fgdb-20foe).
            self.advance()?;
            let (list, filter, map) =
                self.element_scope(inputs.as_deref_mut(), schema, resolve, depth, true)?;
            self.punct(b']', "]")?;
            ReadValueTemplate::Comprehension {
                list: Box::new(list),
                filter: filter.map(Box::new),
                map: map.map(Box::new),
            }
        } else if let Some(kind) = self.list_quantifier()? {
            // any/all/none/single(x IN list WHERE p) (fgdb-20foe).
            self.advance()?;
            self.punct(b'(', "(")?;
            let (list, predicate, _) =
                self.element_scope(inputs.as_deref_mut(), schema, resolve, depth, false)?;
            let predicate = predicate.ok_or(GraphSetTextError {
                offset: self.current.at,
                kind: GraphSetTextErrorKind::Expected("WHERE predicate of a list quantifier"),
            })?;
            self.punct(b')', ")")?;
            ReadValueTemplate::Quantifier {
                kind,
                list: Box::new(list),
                predicate: Box::new(predicate),
            }
        } else if self.take(b'[')? {
            let mut items = Vec::new();
            if !self.take(b']')? {
                loop {
                    self.capacity(
                        items.len(),
                        crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    items.push(self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?);
                    if self.take(b']')? {
                        break;
                    }
                    self.punct(b',', ", or ]")?;
                }
            }
            ReadValueTemplate::List(items)
        } else if self.take(b'{')? {
            // A map literal {k: e, ...} (fgdb-2jw3z), keys sorted and unique.
            let mut entries: Vec<(Box<str>, ReadValueTemplate)> = Vec::new();
            if !self.take(b'}')? {
                loop {
                    self.capacity(
                        entries.len(),
                        crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    let key = self.name()?;
                    self.punct(b':', ":")?;
                    let value = self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?;
                    entries.push((key.text.into(), value));
                    if self.take(b'}')? {
                        break;
                    }
                    self.punct(b',', ", or }")?;
                }
            }
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err(GraphSetTextError {
                    offset: at,
                    kind: GraphSetTextErrorKind::Expected("unique keys in a map literal"),
                });
            }
            let (keys, values): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
            ReadValueTemplate::MapLiteral {
                keys: keys.into_boxed_slice(),
                values,
                guard: None,
            }
        } else if self.is_word("KEYS")
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            self.advance()?;
            self.punct(b'(', "(")?;
            let map =
                self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
            self.punct(b')', ")")?;
            ReadValueTemplate::Keys(Box::new(map))
        } else if let Some(function) = self.list_function()? {
            // head/last/tail/range/reduce (fgdb-20foe).
            self.advance()?;
            self.punct(b'(', "(")?;
            let value = self.list_function_call(
                function,
                inputs.as_deref_mut(),
                schema,
                resolve,
                depth,
                at,
            )?;
            self.punct(b')', ")")?;
            value
        } else if self.is_word("SIZE")
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            self.advance()?;
            self.punct(b'(', "(")?;
            let inner =
                self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
            self.punct(b')', ")")?;
            ReadValueTemplate::Size(Box::new(inner))
        } else if let Some(resolve) = resolve.as_deref_mut() {
            let operand = self
                .resolved_expression(resolve)
                .map_err(expression_error)?;
            self.read_value_template(operand, at)?
        } else if let Some(columns) = inputs.as_deref_mut()
            && matches!(self.current.kind, TokenKind::Word(word)
                if !self.elements.contains(&word)
                    && (self.syntax.variables.iter().any(|name| name.text == word)
                        || self.syntax.visible_edge(word).is_some_and(|(edge, _)| edge.walk.is_none())))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'{'))
        {
            self.map_projection(columns, schema, resolve, depth, at)?
        } else if let Some(columns) = inputs.as_deref_mut() {
            let bare = matches!(self.current.kind, TokenKind::Word(word)
                if !self.elements.contains(&word)
                    && (self.syntax.variables.iter().any(|name| name.text == word)
                    || self.syntax.path.is_some_and(|path| path.text == word)
                    || self.syntax.visible_edge(word).is_some()))
                && !matches!(
                    self.lexer.clone().next()?.kind,
                    TokenKind::Punct(b'.' | b'(')
                );
            if bare {
                let variable = self.any_variable()?;
                ReadValueTemplate::Column(self.mutation_projection(columns, variable, None)?)
            } else {
                let operand = self
                    .mutation_expression(columns)
                    .map_err(expression_error)?;
                self.read_value_template(operand, at)?
            }
        } else {
            let operand = self.row_expression(schema).map_err(expression_error)?;
            self.read_value_template(operand, at)?
        };
        // A lone element is the element's whole value, not a scalar program,
        // so a list, vertex or path element passes through intact.
        if let ReadValueTemplate::Integer { program, .. } = &value
            && let [MutationIntegerTemplateOp::Bound(crate::GraphIntegerOp::Local(offset))] =
                program.as_slice()
        {
            value = ReadValueTemplate::Local(*offset);
        }
        loop {
            // `.key` reads a map entry (fgdb-2jw3z), only on a value that can
            // statically be a map. A graph property read or a WITH-carried
            // vertex's hidden read never reaches this postfix, and on any
            // other value the `.` stays unconsumed, so a vertex column's `.p`
            // past its readable scope still refuses at preparation.
            if self.is_punct(b'.')
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Word(_))
                && map_capable(
                    &value,
                    (inputs.is_none() && resolve.is_none()).then_some(schema),
                )
            {
                self.advance()?;
                let key = self.name()?;
                value = ReadValueTemplate::MapGet {
                    map: Box::new(value),
                    key: key.text.into(),
                };
                continue;
            }
            if !self.take(b'[')? {
                break;
            }
            // `[i]` indexes; `[a..b]`, `[a..]` and `[..b]` slice (fgdb-20foe).
            let from = if self.starts_range_dots()? {
                None
            } else {
                Some(self.read_recursive_value(
                    inputs.as_deref_mut(),
                    schema,
                    resolve,
                    depth + 1,
                )?)
            };
            if self.starts_range_dots()? {
                self.advance()?;
                self.advance()?;
                let to = if self.is_punct(b']') {
                    None
                } else {
                    Some(self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?)
                };
                self.punct(b']', "]")?;
                value = ReadValueTemplate::Slice {
                    list: Box::new(value),
                    from: from.map(Box::new),
                    to: to.map(Box::new),
                };
            } else {
                let index = from.ok_or(GraphSetTextError {
                    offset: self.current.at,
                    kind: GraphSetTextErrorKind::Expected("a list index or slice"),
                })?;
                self.punct(b']', "]")?;
                value = ReadValueTemplate::Index {
                    list: Box::new(value),
                    index: Box::new(index),
                };
            }
        }
        Ok(value)
    }

    fn starts_range_dots(&self) -> Result<bool, GraphPatternTextError> {
        Ok(
            self.is_punct(b'.')
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.')),
        )
    }

    /// HEAD, LAST, TAIL, RANGE or REDUCE directly followed by `(`.
    fn list_function(&self) -> Result<Option<ListFunction>, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        let function = [
            ("HEAD", ListFunction::Head),
            ("LAST", ListFunction::Last),
            ("TAIL", ListFunction::Tail),
            ("RANGE", ListFunction::Range),
            ("REDUCE", ListFunction::Reduce),
        ]
        .into_iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))
        .map(|(_, function)| function);
        let Some(function) = function else {
            return Ok(None);
        };
        Ok(matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')).then_some(function))
    }

    /// The arguments of a list function, after `(` and before `)`. head and
    /// last are index desugars, tail a slice from 1; reduce binds its
    /// accumulator then its element, innermost last.
    #[allow(clippy::type_complexity)]
    fn list_function_call(
        &mut self,
        function: ListFunction,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<
            &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        >,
        depth: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let integer = |value: i64| -> Result<ReadValueTemplate, GraphSetTextError> {
            Ok(ReadValueTemplate::Literal(scalar(
                GqlParameterValue::Int64(value),
                at,
            )?))
        };
        let next = |parser: &mut Self,
                    inputs: Option<&mut Vec<Projection<'a>>>,
                    resolve: &mut Option<_>| {
            parser.read_recursive_value(inputs, schema, resolve, depth + 1)
        };
        Ok(match function {
            ListFunction::Head | ListFunction::Last | ListFunction::Tail => {
                let list = Box::new(next(self, inputs, resolve)?);
                match function {
                    ListFunction::Head => ReadValueTemplate::Index {
                        list,
                        index: Box::new(integer(0)?),
                    },
                    ListFunction::Last => ReadValueTemplate::Index {
                        list,
                        index: Box::new(integer(-1)?),
                    },
                    _ => ReadValueTemplate::Slice {
                        list,
                        from: Some(Box::new(integer(1)?)),
                        to: None,
                    },
                }
            }
            ListFunction::Range => {
                let start = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                self.punct(b',', ",")?;
                let end = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                let step = if self.take(b',')? {
                    Some(Box::new(next(self, inputs, resolve)?))
                } else {
                    None
                };
                ReadValueTemplate::Range { start, end, step }
            }
            ListFunction::Reduce => {
                let accumulator = self.name()?;
                self.punct(b'=', "=")?;
                let init = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                self.punct(b',', ",")?;
                let element = self.name()?;
                self.word("IN")?;
                let list = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                self.punct(b'|', "|")?;
                self.elements.push(accumulator.text);
                self.elements.push(element.text);
                let expr = next(self, inputs, resolve);
                self.elements.truncate(self.elements.len() - 2);
                ReadValueTemplate::Reduce {
                    init,
                    list,
                    expr: Box::new(expr?),
                }
            }
        })
    }

    /// `x IN list (WHERE p)? (| e)?`, with `x` in scope only for `p` and `e`
    /// (fgdb-20foe). The scope is popped on every exit, errors included.
    #[allow(clippy::type_complexity)]
    fn element_scope(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<
            &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        >,
        depth: usize,
        projection: bool,
    ) -> Result<
        (
            ReadValueTemplate,
            Option<ReadValueTemplate>,
            Option<ReadValueTemplate>,
        ),
        GraphSetTextError,
    > {
        let element = self.name()?;
        self.word("IN")?;
        let list = self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
        self.elements.push(element.text);
        let parts = self.element_parts(inputs, schema, resolve, depth, projection);
        self.elements.pop();
        let (filter, map) = parts?;
        Ok((list, filter, map))
    }

    #[allow(clippy::type_complexity)]
    fn element_parts(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<
            &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        >,
        depth: usize,
        projection: bool,
    ) -> Result<(Option<ReadValueTemplate>, Option<ReadValueTemplate>), GraphSetTextError> {
        let filter = if self.take_word("WHERE")? {
            // A predicate parses the whole Boolean expression in every scope,
            // including the operand-only aggregate-resolution scope.
            let at = self.current.at;
            let operand = if let Some(resolve) = resolve.as_deref_mut() {
                self.resolved_predicate(resolve)
            } else if let Some(columns) = inputs.as_deref_mut() {
                self.mutation_expression(columns)
            } else {
                self.row_expression(schema)
            }
            .map_err(expression_error)?;
            Some(self.read_value_template(operand, at)?)
        } else {
            None
        };
        let map = if projection && self.take(b'|')? {
            Some(self.read_recursive_value(inputs, schema, resolve, depth + 1)?)
        } else {
            None
        };
        Ok((filter, map))
    }

    /// `ANY`, `ALL`, `NONE` or `SINGLE` opening `(x IN ...`: the same
    /// lookahead that keeps RETURN/WITH from reading `all(` as `ALL`.
    fn list_quantifier(&self) -> Result<Option<crate::GraphListQuantifier>, GraphPatternTextError> {
        use crate::GraphListQuantifier as Quantifier;
        if !self.starts_list_quantifier()? {
            return Ok(None);
        }
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        let kind = [
            ("ANY", Quantifier::Any),
            ("ALL", Quantifier::All),
            ("NONE", Quantifier::None),
            ("SINGLE", Quantifier::Single),
        ]
        .into_iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))
        .map(|(_, kind)| kind);
        Ok(kind)
    }
}

impl BoundSetTextInput {
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }
    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        if let Some(selection) = &self.selection {
            return selection.checked_arguments(arguments);
        }
        let mut values = Vec::new();
        for (index, spec) in self.parameters.iter().enumerate() {
            let at = self.parameter_offsets[index];
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
                ));
            }
            values.push(value);
        }
        if arguments.len() != values.len() {
            return Err(error(
                self.return_at,
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok(values)
    }

    /// Only callers that checked the COMPLETE native argument table may use
    /// this path. Shared grouped terminals need that same table for HAVING/page.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut input = if self.leading.is_empty() && !self.singleton {
            self.selection
                .as_ref()
                .expect("graph source")
                .bind_values(values)?
                .into()
        } else {
            let mut leading = bind_stages(PreparedGraphSet::singleton(), &self.leading, values)?;
            if let Some(selection) = &self.selection {
                let width = leading.column_types().len();
                leading = leading
                    .cross_join(selection.bind_values(values)?.into())
                    .map_err(|kind| GraphSetTextError {
                        offset: self.return_at,
                        kind: GraphSetTextErrorKind::SetBuild(kind),
                    })?;
                let mut code = Vec::new();
                for &(left, right) in &self.correlations {
                    code.push(crate::GraphSetPredicateOp::Compare {
                        left: crate::GraphSetOperand::Column(left),
                        comparison: IntegerComparison::Equal,
                        right: crate::GraphSetOperand::Column(width + right),
                    });
                    if code.len() > 1 {
                        code.push(crate::GraphSetPredicateOp::And);
                    }
                }
                if !code.is_empty() {
                    leading = leading.filter(&code).map_err(|kind| GraphSetTextError {
                        offset: self.return_at,
                        kind: GraphSetTextErrorKind::FilterBuild(kind),
                    })?;
                }
            }
            leading
        };
        if let Some(projection) = &self.projection {
            input = bind_projection(input, projection, self.quantifier, values, self.return_at)?;
        }
        bind_stages(input, &self.pipeline, values)
    }
}

fn bind_stages(
    mut input: PreparedGraphSet,
    stages: &[ReadStageTemplate],
    values: &[GqlParameterValue],
) -> Result<PreparedGraphSet, GraphSetTextError> {
    for stage in stages {
        input = match stage {
            ReadStageTemplate::Aggregate { at, stage } => stage.bind(input, values, *at)?,
            ReadStageTemplate::Unwind { at, name, value } => input
                .unwind(name.clone(), bind_read_value(value, values)?)
                .map_err(|kind| GraphSetTextError {
                    offset: *at,
                    kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                })?,
            ReadStageTemplate::Call {
                at,
                namespace,
                name,
                arguments,
                names,
                outputs,
                vertices,
                ..
            } => {
                let mut bound = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    bound.push(bind_read_value(argument, values)?);
                }
                input
                    .procedure_call(
                        namespace.clone(),
                        name.clone(),
                        bound,
                        names.clone(),
                        outputs.clone(),
                        vertices.clone(),
                    )
                    .map_err(|kind| GraphSetTextError {
                        offset: *at,
                        kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                    })?
            }
            ReadStageTemplate::Project {
                at,
                projection,
                quantifier,
            } => bind_projection(input, projection, *quantifier, values, *at)?,
            ReadStageTemplate::Filter { at, code } => {
                let code = pipeline::bind_filter(code, Some(values))?;
                input.filter(&code).map_err(|kind| GraphSetTextError {
                    offset: *at,
                    kind: GraphSetTextErrorKind::FilterBuild(kind),
                })?
            }
            ReadStageTemplate::Page {
                at,
                order,
                offset,
                count,
            } => {
                if !order.is_empty() {
                    input = input
                        .with_order_by(order)
                        .map_err(|kind| GraphSetTextError {
                            offset: *at,
                            kind: GraphSetTextErrorKind::OrderBuild(kind),
                        })?;
                }
                input.with_page(
                    pipeline::page_value(offset, values),
                    count
                        .as_ref()
                        .map(|count| pipeline::page_value(count, values)),
                )
            }
        };
    }
    Ok(input)
}

fn bind_projection(
    input: PreparedGraphSet,
    projection: &[ReadProjectionTemplate],
    quantifier: crate::GraphSetQuantifier,
    values: &[GqlParameterValue],
    at: usize,
) -> Result<PreparedGraphSet, GraphSetTextError> {
    let mut columns = Vec::new();
    for output in projection {
        let value = bind_read_value(&output.value, values)?;
        columns.push(GraphSetProjection::new(output.name.clone(), value));
    }
    input
        .project(columns, quantifier)
        .map_err(|kind| GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
        })
}

pub(in crate::graph_text) fn bind_read_value(
    value: &ReadValueTemplate,
    values: &[GqlParameterValue],
) -> Result<GraphSetValue, GraphSetTextError> {
    Ok(match value {
        ReadValueTemplate::Column(input) => GraphSetValue::Column(*input),
        ReadValueTemplate::Literal(value) => GraphSetValue::Literal(value.clone()),
        ReadValueTemplate::Parameter { index, at } => match &values[*index] {
            GqlParameterValue::List(value) => GraphSetValue::Value(value.value().clone()),
            value => GraphSetValue::Literal(scalar(value.clone(), *at)?),
        },
        ReadValueTemplate::Integer { program, at } => GraphSetValue::Integer(
            integer::bind_integer(program, values, *at).map_err(expression_error)?,
        ),
        ReadValueTemplate::List(items) => GraphSetValue::List(
            items
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
        ),
        ReadValueTemplate::Index { list, index } => GraphSetValue::Index {
            list: Box::new(bind_read_value(list, values)?),
            index: Box::new(bind_read_value(index, values)?),
        },
        ReadValueTemplate::In { value, list } => GraphSetValue::In {
            value: Box::new(bind_read_value(value, values)?),
            list: Box::new(bind_read_value(list, values)?),
        },
        ReadValueTemplate::Size(value) => {
            GraphSetValue::Size(Box::new(bind_read_value(value, values)?))
        }
        ReadValueTemplate::Local(offset) => GraphSetValue::Local(*offset),
        ReadValueTemplate::Comprehension { list, filter, map } => {
            let part = |part: &Option<Box<ReadValueTemplate>>| {
                part.as_deref()
                    .map(|part| bind_read_value(part, values).map(Box::new))
                    .transpose()
            };
            GraphSetValue::Comprehension {
                list: Box::new(bind_read_value(list, values)?),
                filter: part(filter)?,
                map: part(map)?,
            }
        }
        ReadValueTemplate::Quantifier {
            kind,
            list,
            predicate,
        } => GraphSetValue::Quantifier {
            kind: *kind,
            list: Box::new(bind_read_value(list, values)?),
            predicate: Box::new(bind_read_value(predicate, values)?),
        },
        ReadValueTemplate::Slice { list, from, to } => {
            let part = |part: &Option<Box<ReadValueTemplate>>| {
                part.as_deref()
                    .map(|part| bind_read_value(part, values).map(Box::new))
                    .transpose()
            };
            GraphSetValue::Slice {
                list: Box::new(bind_read_value(list, values)?),
                from: part(from)?,
                to: part(to)?,
            }
        }
        ReadValueTemplate::Range { start, end, step } => GraphSetValue::Range {
            start: Box::new(bind_read_value(start, values)?),
            end: Box::new(bind_read_value(end, values)?),
            step: step
                .as_deref()
                .map(|step| bind_read_value(step, values).map(Box::new))
                .transpose()?,
        },
        ReadValueTemplate::Reduce { init, list, expr } => GraphSetValue::Reduce {
            init: Box::new(bind_read_value(init, values)?),
            list: Box::new(bind_read_value(list, values)?),
            expr: Box::new(bind_read_value(expr, values)?),
        },
        ReadValueTemplate::MapLiteral {
            keys,
            values: entries,
            guard,
        } => GraphSetValue::MapLiteral {
            keys: keys.clone(),
            values: entries
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
            guard: guard
                .as_deref()
                .map(|guard| bind_read_value(guard, values).map(Box::new))
                .transpose()?,
        },
        ReadValueTemplate::MapGet { map, key } => GraphSetValue::MapGet {
            map: Box::new(bind_read_value(map, values)?),
            key: key.clone(),
        },
        ReadValueTemplate::Keys(map) => {
            GraphSetValue::Keys(Box::new(bind_read_value(map, values)?))
        }
    })
}

#[cfg(test)]
mod graph_scope_tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::{
        GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, PreparedGraphSetText,
    };

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        }
    }

    fn check_schema(text: &str, expected: &[GraphSetColumnType]) {
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(prepared.column_types(), expected, "{text}");
        assert_eq!(
            prepared
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .column_types(),
            expected,
            "{text}"
        );
    }

    #[test]
    fn metadata_scalars_keep_their_types_through_filters_and_alias_shadowing() {
        for text in [
            "MATCH p = (a)-[:R]->{1,2}(b) WITH path_length(p) AS hops \
             WHERE hops > 0 ORDER BY hops DESC LIMIT 1 RETURN hops + 1 AS score",
            "MATCH p = (a)-[:R]->{1,2}(b) WITH path_length(p) AS p RETURN p + 1 AS score",
            "MATCH (a)-[e:R]->(b) WITH type(e) AS name RETURN upper(name) AS value",
        ] {
            check_schema(text, &[GraphSetColumnType::Scalar]);
        }
        check_schema(
            "MATCH p = (a)-[:R]->{1,2}(b) WITH p AS route, nodes(p) AS vertices, \
             edges(p) AS relationships RETURN route, vertices, relationships",
            &[
                GraphSetColumnType::Path,
                GraphSetColumnType::Vertices,
                GraphSetColumnType::Edges,
            ],
        );
        check_schema(
            "MATCH (a) WITH labels(a) AS names RETURN names",
            &[GraphSetColumnType::List],
        );
    }

    #[test]
    fn wildcards_and_implicit_unwind_preserve_named_graph_bindings() {
        for terminal in ["RETURN *", "WITH * RETURN *"] {
            check_schema(
                &format!("MATCH p = (a)-[:R]->{{1,2}}(b) {terminal}"),
                &[
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Path,
                ],
            );
            check_schema(
                &format!("MATCH (a)-[e:R]->(b) {terminal}"),
                &[
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Edge,
                ],
            );
        }
        check_schema(
            "MATCH (a)-[e:R]->(b) UNWIND [1] AS value RETURN e, value",
            &[GraphSetColumnType::Edge, GraphSetColumnType::Any],
        );
        check_schema(
            "MATCH p = (a)-[:R]->{1,2}(b) UNWIND [1] AS value RETURN p, value",
            &[GraphSetColumnType::Path, GraphSetColumnType::Any],
        );
        let prepared =
            PreparedGraphSetText::prepare("MATCH ()-[e:R]->() WITH * RETURN *", symbols).unwrap();
        assert_eq!(prepared.columns(), &["e".to_owned()]);
        assert_eq!(prepared.column_types(), &[GraphSetColumnType::Edge]);
        assert!(prepared.bind_parameters(&GqlParameters::new()).is_ok());
    }

    #[test]
    fn metadata_pipeline_executes_filter_page_and_arithmetic_in_written_order() {
        let query = PreparedGraphSetText::prepare(
            "MATCH p = (a)-[:R]->{0,3}(b) WITH path_length(p) AS hops \
             WHERE hops > 0 ORDER BY hops DESC SKIP 1 LIMIT 1 RETURN hops + 10 AS score",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        // Inject the native source's single path-length column to isolate the
        // row stages from graph traversal. All post-source work stays governed.
        let mut calls = 0;
        let result = query
            .execute_governed(
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                |_, _| {
                    calls += 1;
                    Ok::<_, GqlQueryError<(), ()>>(GqlQueryExecution {
                        value: [0, 1, 3, 2]
                            .into_iter()
                            .map(|value| {
                                GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                                    CanonicalScalar::Int(value),
                                )])
                            })
                            .collect(),
                        rows: GqlExecutionStats {
                            snapshot_records: 4,
                            result_rows: 4,
                        },
                        evaluator: Default::default(),
                    })
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            result.value,
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::Int(12),
            )])]
        );
        assert_eq!(result.rows.result_rows, 1);
    }

    #[test]
    fn graph_identity_aliases_are_not_reclassified_as_scalars() {
        for text in [
            "MATCH p = (a)-[:R]->{1,2}(b) WITH p AS route RETURN route + 1 AS bad",
            "MATCH (a)-[e:R]->(b) WITH e AS edge RETURN edge + 1 AS bad",
            "MATCH p = (a)-[:R]->{1,2}(b) WITH nodes(p) AS vertices \
             RETURN vertices + 1 AS bad",
        ] {
            let mut calls = 0;
            let result = PreparedGraphSetText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            });
            assert!(result.is_err(), "{text}");
            assert_eq!(calls, 0, "{text}");
        }
    }
}
