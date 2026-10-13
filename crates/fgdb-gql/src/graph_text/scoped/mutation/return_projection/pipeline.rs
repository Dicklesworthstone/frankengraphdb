//! WITH is preparation-only relational staging over the SAME native lexer.
//! Runtime receives only native row operators in PreparedGraphSet. There
//! are no graph-slot aliases, synthetic MATCH text, or source calls here.

mod multipart;
mod ordering;
mod selection;

use super::*;
use crate::algebra::{GraphValueOrder, MAX_BOOLEAN_INSTRUCTIONS};
use crate::set_text::{ReadFilterOp, ReadFilterOperand, ReadPageNumber};
use crate::{GraphSetFilterError, GraphSetOperand, GraphSetPredicateOp, GraphSetQuantifier};

const MAX_FILTER_NESTING: usize = 64;
pub(in crate::graph_text) type RowSchema<'a> = Vec<(Name<'a>, GraphSetColumnType)>;

/// Private output names for hidden boundary reads. A fixed set lets each name
/// borrow for the statement's lifetime; a user alias that matches one is
/// skipped, never shadowed.
pub(in crate::graph_text) const BOUNDARY_READ_NAMES: [&str; 16] = [
    "__fg_boundary_0",
    "__fg_boundary_1",
    "__fg_boundary_2",
    "__fg_boundary_3",
    "__fg_boundary_4",
    "__fg_boundary_5",
    "__fg_boundary_6",
    "__fg_boundary_7",
    "__fg_boundary_8",
    "__fg_boundary_9",
    "__fg_boundary_10",
    "__fg_boundary_11",
    "__fg_boundary_12",
    "__fg_boundary_13",
    "__fg_boundary_14",
    "__fg_boundary_15",
];

/// Whether `word`, at nesting depth zero, ends the scope in which a graph-to-row
/// WITH's hidden reads can resolve. The scope ends at a later WITH or UNWIND
/// stage, another MATCH, a CALL, or a set operator. The WITH's own WHERE, its
/// pages and a terminal RETURN are inside the scope. `WITH` after STARTS/ENDS
/// is a text predicate, not a new clause.
pub(in crate::graph_text::scoped::mutation) fn ends_boundary_scope(
    word: &str,
    previous: Option<TokenKind<'_>>,
) -> bool {
    if word.eq_ignore_ascii_case("WITH") {
        return !matches!(previous, Some(TokenKind::Word(previous))
            if previous.eq_ignore_ascii_case("STARTS") || previous.eq_ignore_ascii_case("ENDS"));
    }
    [
        "MATCH",
        "OPTIONAL",
        "UNWIND",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "CALL",
    ]
    .iter()
    .any(|clause| word.eq_ignore_ascii_case(clause))
}

/// The MATCH vertex a boundary output projects unchanged, if any.
fn boundary_binding<'a>(
    head: &GraphProjectionHead<'a>,
    offset: usize,
    alias: &str,
) -> Option<Name<'a>> {
    let (_, ReadValueTemplate::Column(column)) =
        head.outputs.iter().find(|(name, _)| name.text == alias)?
    else {
        return None;
    };
    let input = head.inputs.get(column.checked_sub(offset)?)?;
    (input.property.is_none() && matches!(input.path, None | Some(GraphPathFunction::Edge)))
        .then_some(input.variable)
}

/// Append a hidden boundary output reading head input `column` under the
/// first private name no output uses, and return its output index.
fn hidden_boundary_column<'a>(
    head: &mut GraphProjectionHead<'a>,
    at: usize,
    column: usize,
) -> Result<usize, GraphSetTextError> {
    let Some(&text) = BOUNDARY_READ_NAMES
        .iter()
        .find(|name| head.outputs.iter().all(|(output, _)| output.text != **name))
    else {
        return Err(expected(at, "at most 16 hidden reads after a WITH"));
    };
    head.outputs
        .push((Name { text, at }, ReadValueTemplate::Column(column)));
    Ok(head.outputs.len() - 1)
}

/// Recognize only a complete graph-element map request. Nested map-valued
/// expressions keep their ordinary scalar leaves and are not catalog reads.
pub(super) fn property_map_source<'a>(
    word: &'a str,
    at: usize,
    lexer: &Lexer<'a>,
) -> Result<Option<Name<'a>>, GraphPatternTextError> {
    let mut lookahead = lexer.clone();
    let next = lookahead.next()?;
    if matches!(next.kind, TokenKind::Punct(b'{')) {
        return Ok(Some(Name { text: word, at }));
    }
    if (word.eq_ignore_ascii_case("properties") || word.eq_ignore_ascii_case("keys"))
        && matches!(next.kind, TokenKind::Punct(b'('))
    {
        let argument = lookahead.next()?;
        if let TokenKind::Word(text) = argument.kind
            && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
        {
            return Ok(Some(Name {
                text,
                at: argument.at,
            }));
        }
    }
    Ok(None)
}

/// A complete native metadata call over one named graph binding. Map
/// functions are deliberately excluded: their arguments may be ordinary
/// map-valued rows, and must not be inferred to be vertices. A literal NULL
/// argument is no binding: the scalar compiler answers `labels(null)` and
/// its siblings with NULL (TCK Graph3 [7], Graph4 [3]).
pub(super) fn metadata_source<'a>(
    word: &'a str,
    at: usize,
    lexer: &Lexer<'a>,
) -> Result<Option<(Name<'a>, GraphPathFunction)>, GraphPatternTextError> {
    let Ok(function) = Parser::path_function(Name { text: word, at }) else {
        return Ok(None);
    };
    if !matches!(
        function,
        GraphPathFunction::Labels
            | GraphPathFunction::Type
            | GraphPathFunction::Length
            | GraphPathFunction::Nodes
            | GraphPathFunction::Edges
    ) {
        return Ok(None);
    }
    let mut lookahead = lexer.clone();
    if !matches!(lookahead.next()?.kind, TokenKind::Punct(b'(')) {
        return Ok(None);
    }
    let argument = lookahead.next()?;
    if let TokenKind::Word(text) = argument.kind
        && !text.eq_ignore_ascii_case("NULL")
        && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
    {
        return Ok(Some((
            Name {
                text,
                at: argument.at,
            },
            function,
        )));
    }
    Ok(None)
}

/// Metadata reads may cross a row boundary only through an unchanged value
/// of the required graph domain. Scalars, maps and computed projections do
/// not acquire a graph binding merely because their alias has the same name.
fn boundary_metadata_binding<'a>(
    head: &GraphProjectionHead<'a>,
    offset: usize,
    alias: &str,
    function: GraphPathFunction,
) -> Option<Name<'a>> {
    let (_, ReadValueTemplate::Column(column)) =
        head.outputs.iter().find(|(name, _)| name.text == alias)?
    else {
        return None;
    };
    let input = head.inputs.get(column.checked_sub(offset)?)?;
    if input.property.is_some() {
        return None;
    }
    matches!(
        (function, input.path),
        (GraphPathFunction::Labels, None)
            | (GraphPathFunction::Type, Some(GraphPathFunction::Edge))
            | (
                GraphPathFunction::Length | GraphPathFunction::Nodes | GraphPathFunction::Edges,
                Some(GraphPathFunction::Value)
            )
    )
    .then_some(input.variable)
}

fn expected(at: usize, item: &'static str) -> GraphSetTextError {
    GraphSetTextError {
        offset: at,
        kind: GraphSetTextErrorKind::Expected(item),
    }
}

fn append_stage(
    stages: &mut Vec<ReadStageTemplate>,
    stage: ReadStageTemplate,
    depth: &mut usize,
) -> Result<(), GraphSetTextError> {
    let (at, growth) = match &stage {
        ReadStageTemplate::Aggregate { at, .. } => (*at, 3),
        ReadStageTemplate::Project { at, .. }
        | ReadStageTemplate::Filter { at, .. }
        | ReadStageTemplate::Unwind { at, .. }
        | ReadStageTemplate::Call { at, .. } => (*at, 1),
        ReadStageTemplate::Page { at, computed, .. } => (*at, usize::from(computed.is_some()) * 2),
    };
    *depth += growth;
    if *depth > crate::MAX_GRAPH_SET_DEPTH {
        return Err(GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::SetBuild(crate::GraphSetBuildError::TooDeep {
                limit: crate::MAX_GRAPH_SET_DEPTH,
                observed: *depth,
            }),
        });
    }
    stages.push(stage);
    Ok(())
}
fn emit(
    code: &mut Vec<ReadFilterOp>,
    op: ReadFilterOp,
    at: usize,
) -> Result<(), GraphSetTextError> {
    if code.len() == MAX_BOOLEAN_INSTRUCTIONS {
        return Err(GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::FilterBuild(GraphSetFilterError::TooManyInstructions {
                limit: MAX_BOOLEAN_INSTRUCTIONS,
                observed: code.len() + 1,
            }),
        });
    }
    code.push(op);
    Ok(())
}

impl<'a> Parser<'a> {
    /// `WITH n WHERE n.p = 3 RETURN n.q`: the rest of the graph-to-row
    /// boundary's scope may read a property of a MATCH vertex the WITH
    /// projects as-is (under its own name or an alias). That scope is the
    /// WITH's WHERE, its pages and a terminal RETURN, plain (fgdb-1tgko) or
    /// aggregate: keys, arguments and GROUP BY (fgdb-djlxq). Each such read
    /// becomes a hidden
    /// column of the boundary projection itself, the same graph read
    /// `WITH n.p AS x` makes. Only `alias.property` resolves it, never its
    /// private name. The hidden columns are projected away before a later
    /// stage or another MATCH part can see them, and `*` never does. A
    /// property or metadata value is a function of its graph identity, so
    /// hidden columns never change DISTINCT or multiplicity. Reads in later
    /// stages or through computed or
    /// carried values stay refused as before; a page after the RETURN belongs
    /// to the set parser, which addresses the RETURN's own columns. `offset`
    /// is where this head's graph inputs start in its column space.
    pub(super) fn hoist_boundary_reads(
        &mut self,
        head: &mut GraphProjectionHead<'a>,
        offset: usize,
    ) -> Result<(), GraphSetTextError> {
        if !head.with {
            return Ok(());
        }
        // The scope also covers the expression of the first UNWIND after it,
        // up to its AS: `MATCH (n) UNWIND keys(n) AS k` or `WITH n UNWIND
        // [n.p] AS x` reads a carried element there. An UNWIND sees only what
        // the head projects, never a binding a plain WITH drops.
        let mut unwind = matches!(self.current.kind, TokenKind::Word(word)
            if word.eq_ignore_ascii_case("UNWIND"));
        // Otherwise the token after the projection list is not scanned. If it
        // already ends the scope (a second WITH, MATCH ...), there is none.
        if let TokenKind::Word(word) = self.current.kind
            && !unwind
            && ends_boundary_scope(word, None)
        {
            return Ok(());
        }
        let visible = head.outputs.len();
        let mut reads = Vec::new();
        // A plain WITH's own WHERE and pages (everything before the RETURN)
        // may also read the MATCH bindings it drops, as openCypher allows.
        // Those reads are hidden columns too, but scoped to that part.
        let mut scoped: Vec<(&'a str, Option<&'a str>, usize)> = Vec::new();
        let mut binders: Vec<&'a str> = Vec::new();
        let mut before_return = head.plain && !unwind;
        let mut functions = Vec::new();
        let mut lexer = self.lexer.clone();
        // The three tokens before the current one, oldest first.
        let mut window: [Option<TokenKind<'a>>; 3] = [None; 3];
        let mut depth = 0_usize;
        loop {
            let token = lexer.next()?;
            let after_dot = matches!(window[2], Some(TokenKind::Punct(b'.')));
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        break;
                    };
                    depth = outer;
                }
                TokenKind::Word(word)
                    if unwind && depth == 0 && !after_dot && word.eq_ignore_ascii_case("AS") =>
                {
                    break;
                }
                TokenKind::Word(word)
                    if !unwind
                        && depth == 0
                        && !after_dot
                        && word.eq_ignore_ascii_case("UNWIND")
                        && !matches!(window[2], Some(TokenKind::Word(previous))
                            if previous.eq_ignore_ascii_case("AS")) =>
                {
                    unwind = true;
                    before_return = false;
                }
                TokenKind::Word(word)
                    if depth == 0 && !after_dot && ends_boundary_scope(word, window[2]) =>
                {
                    break;
                }
                TokenKind::Word(word)
                    if depth == 0 && !after_dot && word.eq_ignore_ascii_case("RETURN") =>
                {
                    before_return = false;
                }
                TokenKind::Word(word) if !after_dot => {
                    let next = lexer.clone().next()?.kind;
                    if before_return
                        && !matches!(next, TokenKind::Punct(b'.' | b'(' | b'{' | b':'))
                        && head.outputs.iter().all(|(output, _)| output.text != word)
                    {
                        // `[x IN list ...]` and quantifiers bind a local that
                        // shadows any MATCH binding of the same name.
                        if matches!(window[2], Some(TokenKind::Punct(b'[' | b'(')))
                            && matches!(next, TokenKind::Word(next) if next.eq_ignore_ascii_case("IN"))
                        {
                            binders.push(word);
                        } else if !scoped
                            .iter()
                            .any(|&(name, key, _)| name == word && key.is_none())
                            && let Some(variable) =
                                self.visible_graph_bindings().find(|name| name.text == word)
                        {
                            let input =
                                self.mutation_projection(&mut head.inputs, variable, None)?;
                            let column = hidden_boundary_column(head, token.at, offset + input)?;
                            scoped.push((word, None, column));
                        }
                    }
                    if let Some((source, function)) = metadata_source(word, token.at, &lexer)?
                        && let Some(variable) =
                            boundary_metadata_binding(head, offset, source.text, function)
                        && !functions
                            .iter()
                            .any(|&(alias, known, _)| alias == source.text && known == function)
                    {
                        let name = BOUNDARY_READ_NAMES
                            .iter()
                            .copied()
                            .find(|name| {
                                head.outputs.iter().all(|(output, _)| output.text != *name)
                            })
                            .ok_or_else(|| {
                                expected(source.at, "at most 16 carried-element reads after a WITH")
                            })?;
                        let input = self.projection_slot(
                            &mut head.inputs,
                            Projection {
                                variable,
                                property: None,
                                path: Some(function),
                            },
                        )?;
                        head.outputs.push((
                            Name {
                                text: name,
                                at: token.at,
                            },
                            ReadValueTemplate::Column(offset + input),
                        ));
                        functions.push((source.text, function, head.outputs.len() - 1));
                    }
                    if let Some(source) = property_map_source(word, token.at, &lexer)?
                        && let Some(variable) = boundary_binding(head, offset, source.text)
                        && !reads.iter().any(|&(alias, key, _): &(_, Name<'_>, _)| {
                            alias == source.text && key.text == "*"
                        })
                    {
                        let name = BOUNDARY_READ_NAMES
                            .iter()
                            .copied()
                            .find(|name| {
                                head.outputs.iter().all(|(output, _)| output.text != *name)
                            })
                            .ok_or_else(|| {
                                expected(
                                    source.at,
                                    "at most 16 carried-element property reads after a WITH",
                                )
                            })?;
                        let input = self.property_map_projection(&mut head.inputs, variable)?;
                        head.outputs.push((
                            Name {
                                text: name,
                                at: source.at,
                            },
                            ReadValueTemplate::Column(offset + input),
                        ));
                        reads.push((
                            source.text,
                            Name {
                                text: "*",
                                at: source.at,
                            },
                            head.outputs.len() - 1,
                        ));
                    }
                }
                TokenKind::Word(property) if after_dot => {
                    let chained = matches!(window[0], Some(TokenKind::Punct(b'.')));
                    if let (Some(TokenKind::Word(alias)), false) = (window[1], chained)
                        && let Some(variable) = boundary_binding(head, offset, alias)
                        && !reads.iter().any(|&(read, key, _): &(_, Name<'_>, _)| {
                            read == alias && key.text == property
                        })
                    {
                        let Some(&name) = BOUNDARY_READ_NAMES.iter().find(|name| {
                            head.outputs.iter().all(|(output, _)| output.text != **name)
                        }) else {
                            return Err(expected(
                                token.at,
                                "at most 16 carried-vertex property reads after a WITH",
                            ));
                        };
                        let key = Name {
                            text: property,
                            at: token.at,
                        };
                        let input =
                            self.mutation_projection(&mut head.inputs, variable, Some(key))?;
                        head.outputs.push((
                            Name {
                                text: name,
                                at: token.at,
                            },
                            ReadValueTemplate::Column(offset + input),
                        ));
                        reads.push((alias, key, head.outputs.len() - 1));
                    } else if before_return
                        && !chained
                        && let Some(TokenKind::Word(alias)) = window[1]
                        && head.outputs.iter().all(|(output, _)| output.text != alias)
                        && !scoped
                            .iter()
                            .any(|&(name, key, _)| name == alias && key == Some(property))
                        && let Some(variable) = self
                            .visible_graph_bindings()
                            .find(|name| name.text == alias)
                    {
                        let key = Name {
                            text: property,
                            at: token.at,
                        };
                        let input =
                            self.mutation_projection(&mut head.inputs, variable, Some(key))?;
                        let column = hidden_boundary_column(head, token.at, offset + input)?;
                        scoped.push((alias, Some(property), column));
                    }
                }
                _ => {}
            }
            window = [window[1], window[2], Some(token.kind)];
        }
        scoped.retain(|&(name, _, _)| !binders.contains(&name));
        if head.outputs.len() != visible {
            self.boundary_reads = Some(BoundaryReads {
                visible,
                width: head.outputs.len(),
                reads,
                functions,
                grouped: Vec::new(),
                scoped,
            });
        }
        Ok(())
    }

    /// Resolve a carried property or native metadata function to its hidden
    /// source column when `width` is the boundary row. The complete expression
    /// is consumed on success; `None` consumes nothing.
    pub(in crate::graph_text) fn boundary_read(
        &mut self,
        width: usize,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let (Some(boundary), TokenKind::Word(alias)) = (&self.boundary_reads, self.current.kind)
        else {
            return Ok(None);
        };
        // A comprehension, quantifier or reduce local shadows every hidden
        // read of the same name, whichever scan found the name.
        if boundary.width != width || self.elements.contains(&alias) {
            return Ok(None);
        }
        if let Some((source, function)) = metadata_source(alias, self.current.at, &self.lexer)? {
            if self.elements.contains(&source.text) {
                return Ok(None);
            }
            if let Some(&(_, _, column)) = boundary
                .functions
                .iter()
                .find(|&&(read, known, _)| read == source.text && known == function)
            {
                for _ in 0..4 {
                    self.advance()?;
                }
                return Ok(Some(column));
            }
            return Ok(None);
        }
        let mut lookahead = self.lexer.clone();
        let next = lookahead.next()?.kind;
        if !matches!(next, TokenKind::Punct(b'.')) {
            // A binding a plain WITH dropped, read by its own WHERE or page.
            if matches!(next, TokenKind::Punct(b'(' | b'{')) {
                return Ok(None);
            }
            let Some(&(_, _, column)) =
                (boundary.scoped.iter()).find(|&&(name, key, _)| name == alias && key.is_none())
            else {
                return Ok(None);
            };
            self.advance()?;
            return Ok(Some(column));
        }
        let TokenKind::Word(property) = lookahead.next()?.kind else {
            return Ok(None);
        };
        let Some(column) = (boundary.reads.iter())
            .find(|&&(read, key, _)| read == alias && key.text == property)
            .map(|&(_, _, column)| column)
            .or_else(|| {
                (boundary.scoped.iter())
                    .find(|&&(name, key, _)| name == alias && key == Some(property))
                    .map(|&(_, _, column)| column)
            })
        else {
            return Ok(None);
        };
        for _ in 0..3 {
            self.advance()?;
        }
        Ok(Some(column))
    }

    /// A grouping or DISTINCT WITH's own ORDER BY repeating one of its items'
    /// expressions, token for token, at the current token: that item's output
    /// column over a row of `width`, consuming the repeat (openCypher: `WITH
    /// a.name AS name, count(*) AS c ORDER BY a.name`). Within a group the
    /// repeat and the output are the same value. Its WHERE resolves leaves
    /// over no row schema, so it never sees these. `None` consumes nothing.
    /// Out of line: the scalar operand reader that calls it repeats once per
    /// nested parenthesis, and these lexers must not widen that frame.
    #[inline(never)]
    pub(in crate::graph_text) fn projected_source(
        &mut self,
        width: usize,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let Some((row, sources)) = self.projected_sources.as_ref() else {
            return Ok(None);
        };
        // A comprehension, quantifier or reduce local is never an item's name.
        if *row != width
            || matches!(self.current.kind, TokenKind::Word(word) if self.elements.contains(&word))
        {
            return Ok(None);
        }
        let mut found = None;
        for &(start, end, column) in sources {
            let mut item = crate::graph_text::Lexer {
                text: self.lexer.text,
                at: start,
                tokens: 0,
            };
            let mut here = self.lexer.clone();
            let mut token = self.current;
            let mut length = 0_usize;
            let repeated = loop {
                let expected = item.next()?;
                if expected.at >= end {
                    break length > 0;
                }
                if expected.kind != token.kind {
                    break false;
                }
                length += 1;
                token = here.next()?;
            };
            if repeated {
                found = Some((length, column));
                break;
            }
        }
        let Some((length, column)) = found else {
            return Ok(None);
        };
        for _ in 0..length {
            self.advance()?;
        }
        Ok(Some(column))
    }

    /// Project hidden boundary reads away (fgdb-1tgko) before a scope they
    /// must not reach: a later WITH or UNWIND stage, the next MATCH part, or
    /// the end of the statement. A no-op when this row carries none.
    fn drop_boundary_reads(
        &mut self,
        schema: &mut RowSchema<'a>,
        stages: &mut Vec<ReadStageTemplate>,
        depth: &mut usize,
        at: usize,
    ) -> Result<(), GraphSetTextError> {
        if !self
            .boundary_reads
            .as_ref()
            .is_some_and(|boundary| boundary.width == schema.len())
        {
            return Ok(());
        }
        let Some(boundary) = self.boundary_reads.take() else {
            return Ok(());
        };
        let projection = schema[..boundary.visible]
            .iter()
            .enumerate()
            .map(|(index, (name, _))| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(index),
            })
            .collect();
        append_stage(
            stages,
            ReadStageTemplate::Project {
                at,
                projection,
                quantifier: GraphSetQuantifier::All,
            },
            depth,
        )?;
        schema.truncate(boundary.visible);
        Ok(())
    }

    /// The columns a name or `*` may resolve to: all of `schema`, minus any
    /// hidden boundary reads this row carries (their private names never
    /// resolve by text).
    pub(in crate::graph_text) fn visible_width(
        &self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> usize {
        self.boundary_reads
            .as_ref()
            .filter(|boundary| boundary.width == schema.len())
            .map_or(schema.len(), |boundary| boundary.visible)
    }

    pub(in crate::graph_text) fn row_pipeline(
        &mut self,
        schema: RowSchema<'a>,
    ) -> Result<Vec<ReadStageTemplate>, GraphSetTextError> {
        self.row_pipeline_output(schema, false)
    }

    /// A write's owned final-state rows may end in an aggregate RETURN.
    /// Keep ordinary read dispatch on its existing, separately typed lanes.
    pub(in crate::graph_text::scoped::mutation) fn write_row_pipeline(
        &mut self,
        schema: RowSchema<'a>,
    ) -> Result<Vec<ReadStageTemplate>, GraphSetTextError> {
        self.row_pipeline_output(schema, true)
    }

    fn row_pipeline_output(
        &mut self,
        schema: RowSchema<'a>,
        aggregate_return: bool,
    ) -> Result<Vec<ReadStageTemplate>, GraphSetTextError> {
        let (mut stages, schema, mut depth) = self.row_pipeline_prefix(schema)?;
        // A write tail that meets the next write of a chain ends here: that
        // write reads this row, hidden boundary reads included.
        if self.chain_write && self.at_chained_write()? {
            self.chained_row = Some(schema);
            return Ok(stages);
        }
        if aggregate_return && self.return_has_aggregate()? {
            let (stage, schema) = self.return_aggregate_stage(&schema)?;
            append_stage(&mut stages, stage, &mut depth)?;
            // This page belongs to the completed groups, not their input.
            // All input expressions still execute under the statement's
            // original work, scratch and cancellation allowance.
            if let Some(page) = self.row_expression_page(&schema)? {
                append_stage(&mut stages, page, &mut depth)?;
            }
            self.boundary_reads = None;
            self.projected_sources = None;
            return Ok(stages);
        }
        let at = self.current.at;
        self.word("RETURN")?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_all_quantifier()?;
        }
        let quantifier = if distinct {
            GraphSetQuantifier::Distinct
        } else {
            GraphSetQuantifier::All
        };
        let (mut projection, _) = self.row_projection(&schema, true)?;
        let page = self.return_expression_page(&schema, &mut projection, distinct)?;
        // The terminal RETURN is the last reader of any hidden boundary read.
        self.boundary_reads = None;
        append_stage(
            &mut stages,
            ReadStageTemplate::Project {
                at,
                projection,
                quantifier,
            },
            &mut depth,
        )?;
        if let Some(page) = page {
            append_stage(&mut stages, page, &mut depth)?;
        }
        Ok(stages)
    }

    /// Parse complete row stages but leave the next clause to its typed owner.
    /// Reads and CREATE/INSERT share this WITH/UNWIND grammar, including exact
    /// aggregation; no caller rewrites a prefix into a synthetic RETURN query.
    pub(in crate::graph_text) fn row_pipeline_prefix(
        &mut self,
        mut schema: RowSchema<'a>,
    ) -> Result<(Vec<ReadStageTemplate>, RowSchema<'a>, usize), GraphSetTextError> {
        let mut stages = Vec::new();
        // The MATCH input and first WITH projection each own one relational node.
        let mut depth = 2;
        loop {
            if let Some(page) = self.row_expression_page(&schema)? {
                append_stage(&mut stages, page, &mut depth)?;
            }
            let at = self.current.at;
            if self.is_word("UNWIND") {
                self.word("UNWIND")?;
                // The UNWIND expression may read a carried element's hidden
                // boundary columns (hoist_boundary_reads), never a binding a
                // plain WITH dropped nor an item repeat. The hidden columns
                // end right after it; the unwound column stays.
                if let Some(boundary) = self.boundary_reads.as_mut() {
                    boundary.scoped.clear();
                }
                self.projected_sources = None;
                let carried = self
                    .boundary_reads
                    .as_ref()
                    .filter(|boundary| boundary.width == schema.len())
                    .map(|boundary| boundary.visible);
                let stage = self.unwind_stage(&mut schema, at)?;
                append_stage(&mut stages, stage, &mut depth)?;
                if let Some(visible) = carried {
                    self.boundary_reads = None;
                    let unwound = schema.len() - 1;
                    let projection = (0..visible)
                        .chain([unwound])
                        .map(|index| ReadProjectionTemplate {
                            name: schema[index].0.text.to_owned(),
                            value: ReadValueTemplate::Column(index),
                        })
                        .collect();
                    append_stage(
                        &mut stages,
                        ReadStageTemplate::Project {
                            at,
                            projection,
                            quantifier: GraphSetQuantifier::All,
                        },
                        &mut depth,
                    )?;
                    let column = schema[unwound];
                    schema.truncate(visible);
                    schema.push(column);
                }
                continue;
            }
            if self.take_word("WHERE")? {
                self.row_selection(&schema, &mut stages, &mut depth, at)?;
                // A page written after WHERE applies to the filtered rows.
                // Keep any earlier page on its input: moving either page across
                // this filter changes which occurrences survive.
                if let Some(page) = self.row_expression_page(&schema)? {
                    append_stage(&mut stages, page, &mut depth)?;
                }
                // UNWIND starts a new row stage, just as WITH does. Let the
                // next iteration consume it without accepting a second WHERE
                // or page on this same completed stage.
                if self.is_word("UNWIND") {
                    continue;
                }
            }
            // A plain WITH's dropped bindings end with its WHERE and pages;
            // a RETURN or a later stage never resolves them. So do a grouping
            // or DISTINCT WITH's repeatable item expressions.
            if let Some(boundary) = self.boundary_reads.as_mut() {
                boundary.scoped.clear();
            }
            self.projected_sources = None;
            let at = self.current.at;
            if !self.is_word("WITH") {
                // A terminal RETURN, plain or aggregate, resolves the hidden
                // reads; the next MATCH part or the end must not see them.
                // A write after WITH reads them too (fgdb-sl4ym).
                if !self.is_word("RETURN") && !self.write_prefix {
                    self.drop_boundary_reads(&mut schema, &mut stages, &mut depth, at)?;
                }
                break;
            }
            if self.with_has_aggregate()? {
                let (stage, next_schema) = self.with_aggregate_stage(&schema)?;
                append_stage(&mut stages, stage, &mut depth)?;
                schema = next_schema;
                continue;
            }
            self.drop_boundary_reads(&mut schema, &mut stages, &mut depth, at)?;
            self.word("WITH")?;
            let distinct = self.take_word("DISTINCT")?;
            if !distinct {
                self.take_all_quantifier()?;
            }
            let quantifier = if distinct {
                GraphSetQuantifier::Distinct
            } else {
                GraphSetQuantifier::All
            };
            let (projection, next_schema) = self.row_projection(&schema, false)?;
            append_stage(
                &mut stages,
                ReadStageTemplate::Project {
                    at,
                    projection,
                    quantifier,
                },
                &mut depth,
            )?;
            schema = next_schema;
        }
        Ok((stages, schema, depth))
    }

    /// A RETURN (`returning`) names an unaliased computed value by its
    /// source text, as openCypher does; a WITH binding needs its AS name.
    fn row_projection(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        returning: bool,
    ) -> Result<(Vec<ReadProjectionTemplate>, RowSchema<'a>), GraphSetTextError> {
        let mut projection = Vec::new();
        let mut next_schema: RowSchema<'a> = Vec::new();
        if self.take(b'*')? {
            let visible = self.visible_width(schema);
            for (index, &(name, kind)) in schema[..visible].iter().enumerate() {
                projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(index),
                });
                next_schema.push((name, kind));
            }
        } else {
            loop {
                self.capacity(
                    projection.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = self.current.at;
                let implicit = if returning {
                    None
                } else {
                    self.implicit_with_binding()?
                };
                let value = self.read_row_value(schema, 0)?;
                let end = self.current.at;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if !returning {
                    implicit
                        .filter(|(_, boundary)| {
                            *boundary == end && matches!(value, ReadValueTemplate::Column(_))
                        })
                        .map(|(name, _)| name)
                        .ok_or_else(|| expected(at, "AS alias for a computed row value"))?
                } else {
                    // An unaliased RETURN item is named by its exact text,
                    // as in openCypher: `n.p` over a carried `n` is "n.p".
                    self.source_name(at, end)
                };
                if next_schema.iter().any(|(name, _)| name.text == alias.text) {
                    return Err(error(
                        alias.at,
                        GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                    )
                    .into());
                }
                let types: Vec<_> = schema.iter().map(|(_, kind)| *kind).collect();
                let kind = value.column_type(&types, &self.syntax.parameters);
                projection.push(ReadProjectionTemplate {
                    name: alias.text.to_owned(),
                    value,
                });
                next_schema.push((alias, kind));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok((projection, next_schema))
    }

    pub(in crate::graph_text) fn unwind_stage(
        &mut self,
        schema: &mut RowSchema<'a>,
        at: usize,
    ) -> Result<ReadStageTemplate, GraphSetTextError> {
        if let TokenKind::Parameter(name) = self.current.kind {
            self.parameter_types
                .entry(name.to_owned())
                .or_insert(GqlParameterType::List);
        }
        let value = self.read_row_value(schema, 0)?;
        self.word("AS")?;
        let alias = self.name()?;
        if schema.iter().any(|(name, _)| name.text == alias.text) {
            return Err(expected(alias.at, "new UNWIND alias"));
        }
        self.capacity(
            schema.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        schema.push((alias, GraphSetColumnType::Any));
        Ok(ReadStageTemplate::Unwind {
            at,
            name: alias.text.to_owned(),
            value,
        })
    }

    /// `CALL namespace.name(arguments) YIELD output [AS alias], ...` after the
    /// CALL keyword. Arguments are constant row values (literals, parameters),
    /// either all positional or all named (`name => value`, each name once);
    /// each yielded output becomes an Any-domain column, as UNWIND's does.
    /// The host resolves the procedure and checks its arguments at execution,
    /// except schema names (`symbol_argument`), which resolve with the pattern.
    pub(in crate::graph_text) fn call_stage(
        &mut self,
        schema: &mut RowSchema<'a>,
        at: usize,
    ) -> Result<ReadStageTemplate, GraphSetTextError> {
        let namespace = self.name()?;
        self.punct(b'.', "procedure name after the namespace")?;
        let name = self.name()?;
        self.punct(b'(', "procedure arguments")?;
        let mut arguments = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let mut offsets = Vec::new();
        if !self.take(b')')? {
            loop {
                let named = self.argument_name()?;
                match named {
                    Some(argument) if names.len() == arguments.len() => {
                        if names.iter().any(|seen| seen == argument.text) {
                            return Err(expected(argument.at, "a procedure argument named once"));
                        }
                        names.push(argument.text.to_owned());
                    }
                    Some(argument) => {
                        return Err(expected(argument.at, "a positional procedure argument"));
                    }
                    None if !names.is_empty() => {
                        return Err(expected(self.current.at, "a named procedure argument"));
                    }
                    None => {}
                }
                offsets.push(self.current.at);
                arguments.push(self.read_row_value(schema, 0)?);
                if self.take(b',')? {
                    continue;
                }
                self.punct(b')', "closing procedure argument list")?;
                break;
            }
        }
        self.word("YIELD")?;
        let mut outputs = Vec::new();
        loop {
            let output = self.name()?;
            let alias = if self.take_word("AS")? {
                self.name()?
            } else {
                output
            };
            if schema.iter().any(|(name, _)| name.text == alias.text) {
                return Err(expected(alias.at, "new YIELD column name"));
            }
            self.capacity(
                schema.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            schema.push((alias, GraphSetColumnType::Any));
            outputs.push((output.text.to_owned(), alias.text.to_owned()));
            if !self.take(b',')? {
                break;
            }
        }
        Ok(ReadStageTemplate::Call {
            at,
            namespace: namespace.text.to_owned(),
            name: name.text.to_owned(),
            arguments,
            names,
            offsets,
            outputs,
            // A following MATCH (written or implied) marks its vertex uses.
            vertices: Vec::new(),
            graph_projection: None,
        })
    }

    /// `name =>` opening a named procedure argument, consumed when present.
    /// The arrow is one token pair with nothing between `=` and `>`.
    fn argument_name(&mut self) -> Result<Option<Name<'a>>, GraphPatternTextError> {
        let TokenKind::Word(text) = self.current.kind else {
            return Ok(None);
        };
        let mut lexer = self.lexer.clone();
        let equals = lexer.next()?;
        let arrow = lexer.next()?;
        if !matches!(equals.kind, TokenKind::Punct(b'='))
            || !matches!(arrow.kind, TokenKind::Punct(b'>'))
            || arrow.at != equals.at + 1
        {
            return Ok(None);
        }
        let argument = Name {
            text,
            at: self.current.at,
        };
        for _ in 0..3 {
            self.advance()?;
        }
        Ok(Some(argument))
    }

    /// `CALL ... YIELD n, score RETURN n.name, score` (fgdb-luq0b): the CALL
    /// outputs the first scope after the leading stages reads as `n.p` or
    /// `labels(n)`, in
    /// first-read order. That scope is its WHERE, pages and RETURN, or the
    /// items of a WITH that directly follows (the graph-to-row boundary, as
    /// after MATCH). Each is read through the identity `MATCH (n)` the
    /// statement could have written; the host refuses a non-vertex output.
    /// UNWIND columns are never implied: no host types their values.
    /// Map functions/projections stay row operations unless another graph
    /// demand establishes the binding; a procedure may legitimately yield a map.
    pub(in crate::graph_text) fn implied_call_vertices(
        &self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        leading: &[ReadStageTemplate],
    ) -> Result<Vec<Name<'a>>, GraphPatternTextError> {
        let Some(ReadStageTemplate::Call { outputs, .. }) = leading.first() else {
            return Ok(Vec::new());
        };
        let yielded = &schema[..outputs.len()];
        let mut implied: Vec<Name<'a>> = Vec::new();
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut previous = None;
        if matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("WITH")) {
            previous = Some(token.kind);
            token = lexer.next()?;
        }
        let mut depth = 0_usize;
        loop {
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        break;
                    };
                    depth = outer;
                }
                TokenKind::Word(word) if !matches!(previous, Some(TokenKind::Punct(b'.'))) => {
                    if depth == 0 && ends_boundary_scope(word, previous) {
                        break;
                    }
                    let source = if matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'.')) {
                        Some(Name {
                            text: word,
                            at: token.at,
                        })
                    } else {
                        metadata_source(word, token.at, &lexer)?
                            .filter(|(_, function)| *function == GraphPathFunction::Labels)
                            .map(|(source, _)| source)
                    };
                    if let Some(source) = source
                        && yielded.iter().any(|(name, _)| name.text == source.text)
                        && !implied.iter().any(|seen| seen.text == source.text)
                    {
                        implied.push(source);
                    }
                }
                _ => {}
            }
            previous = Some(token.kind);
            token = lexer.next()?;
        }
        Ok(implied)
    }

    pub(in crate::graph_text) fn row_page(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Option<ReadStageTemplate>, GraphSetTextError> {
        self.row_page_sourced(schema, &[])
    }

    /// `row_page` where `sources[i]` is the source spelling of output `i`, so
    /// a write RETURN may order by `n.p` whether or not the item was aliased.
    /// A key matching an output name keeps that meaning.
    pub(in crate::graph_text) fn row_page_sourced(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        sources: &[Name<'a>],
    ) -> Result<Option<ReadStageTemplate>, GraphSetTextError> {
        let at = self.current.at;
        let mut present = false;
        let mut order = Vec::new();
        if self.take_word("ORDER")? {
            present = true;
            self.word("BY")?;
            loop {
                // `alias.property` of a carried MATCH vertex orders by its
                // hidden boundary read; every other key is a visible column.
                let column = if let Some(column) = self.boundary_read(schema.len())? {
                    column
                } else if let Some(column) = self.sourced_key(sources)? {
                    column
                } else {
                    let visible = self.visible_width(schema);
                    let name = self.name()?;
                    schema[..visible]
                        .iter()
                        .position(|(alias, _)| alias.text == name.text)
                        .ok_or_else(|| expected(name.at, "projected WITH column"))?
                };
                // A later key on a column already sorted on cannot change the
                // order: rows equal there tie on both. openCypher keeps the
                // first direction (TCK WithOrderBy3 [7], `a ASC, a DESC`); the
                // repeat's own direction is still read.
                let repeated = order
                    .iter()
                    .any(|key: &GraphValueOrder| key.column == column);
                let descending = self.take_word("DESC")? || self.take_word("DESCENDING")?;
                if !descending && !self.take_word("ASC")? {
                    self.take_word("ASCENDING")?;
                }
                let nulls_first = if self.take_word("NULLS")? {
                    if self.take_word("FIRST")? {
                        true
                    } else if self.take_word("LAST")? {
                        false
                    } else {
                        return Err(expected(self.current.at, "FIRST or LAST"));
                    }
                } else {
                    false
                };
                if !repeated {
                    order.push(GraphValueOrder {
                        column,
                        descending,
                        nulls_first,
                    });
                }
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
        Ok(present.then_some(ReadStageTemplate::Page {
            at,
            order,
            offset,
            count,
            computed: None,
        }))
    }

    /// The output whose source is spelled `alias.property` at the current
    /// token, consuming all three tokens; `None` consumes nothing.
    fn sourced_key(
        &mut self,
        sources: &[Name<'a>],
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(alias) = self.current.kind else {
            return Ok(None);
        };
        let mut lookahead = self.lexer.clone();
        if !matches!(lookahead.next()?.kind, TokenKind::Punct(b'.')) {
            return Ok(None);
        }
        let TokenKind::Word(property) = lookahead.next()?.kind else {
            return Ok(None);
        };
        let Some(column) = sources.iter().position(|source| {
            source
                .text
                .split_once('.')
                .is_some_and(|(left, right)| left.trim() == alias && right.trim() == property)
        }) else {
            return Ok(None);
        };
        for _ in 0..3 {
            self.advance()?;
        }
        Ok(Some(column))
    }

    fn row_page_number(&mut self) -> Result<ReadPageNumber, GraphPatternTextError> {
        Ok(match self.number(GqlParameterType::UInt64)? {
            Number::Literal(GqlParameterValue::UInt64(value)) => ReadPageNumber::Literal(value),
            Number::Parameter(index) => ReadPageNumber::Parameter(index),
            _ => unreachable!("UInt64 page parser returns its declared domain"),
        })
    }

    pub(super) fn row_disjunction(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
        code: &mut Vec<ReadFilterOp>,
    ) -> Result<(), GraphSetTextError> {
        self.row_conjunction(schema, depth, code)?;
        while self.is_word("OR") {
            let at = self.current.at;
            self.advance()?;
            self.row_conjunction(schema, depth, code)?;
            emit(code, ReadFilterOp::Or, at)?;
        }
        Ok(())
    }
    fn row_conjunction(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
        code: &mut Vec<ReadFilterOp>,
    ) -> Result<(), GraphSetTextError> {
        self.row_predicate(schema, depth, code)?;
        while self.is_word("AND") {
            let at = self.current.at;
            self.advance()?;
            self.row_predicate(schema, depth, code)?;
            emit(code, ReadFilterOp::And, at)?;
        }
        Ok(())
    }
    fn row_predicate(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
        code: &mut Vec<ReadFilterOp>,
    ) -> Result<(), GraphSetTextError> {
        let at = self.current.at;
        if depth > MAX_FILTER_NESTING {
            return Err(expected(at, "bounded row predicate nesting"));
        }
        // An admitted alias named not wins when used as a comparison operand.
        let alias = matches!(self.current.kind, TokenKind::Word(word)
            if schema.iter().any(|(name, _)| name.text == word));
        if self.is_word("NOT") && !alias {
            self.advance()?;
            self.row_predicate(schema, depth + 1, code)?;
            return emit(code, ReadFilterOp::Not, at);
        }
        if self.take(b'(')? {
            self.row_disjunction(schema, depth + 1, code)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let left = self.row_filter_operand(schema)?;
        if self.take_word("IS")? {
            let negate = self.take_word("NOT")?;
            self.word("NULL")?;
            return emit(
                code,
                ReadFilterOp::IsNull {
                    operand: left,
                    is_null: !negate,
                },
                at,
            );
        }
        if self.is_punct(b'=') || self.is_punct(b'!') || self.is_punct(b'<') || self.is_punct(b'>')
        {
            let comparison = self.comparison()?;
            let right = self.row_filter_operand(schema)?;
            return emit(
                code,
                ReadFilterOp::Compare {
                    left,
                    comparison,
                    right,
                },
                at,
            );
        }
        let truth = match left {
            ReadFilterOperand::Literal(value) => match value.value() {
                CanonicalScalar::Null => None,
                CanonicalScalar::Bool(value) => Some(*value),
                _ => return Err(expected(at, "row comparison or IS NULL")),
            },
            _ => return Err(expected(at, "row comparison or IS NULL")),
        };
        emit(code, ReadFilterOp::Truth(truth), at)
    }
    fn row_filter_operand(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ReadFilterOperand, GraphSetTextError> {
        let at = self.current.at;
        let operand = self.row_operand(schema)?;
        Ok(match operand {
            Operand::Column(column) => ReadFilterOperand::Column(column),
            Operand::Literal(value) => ReadFilterOperand::Literal(value),
            Operand::Number(Number::Literal(value)) => {
                ReadFilterOperand::Literal(scalar(value, at)?)
            }
            Operand::Number(Number::Parameter(index)) => ReadFilterOperand::Parameter { index, at },
            Operand::Integer { .. } => unreachable!("row operands are not arithmetic expressions"),
        })
    }
}

pub(in crate::graph_text) fn page_value(
    number: &ReadPageNumber,
    values: &[GqlParameterValue],
) -> u64 {
    match number {
        ReadPageNumber::Literal(value) => *value,
        ReadPageNumber::Parameter(index) => match &values[*index] {
            GqlParameterValue::UInt64(value) => *value,
            _ => unreachable!("the complete parameter map was checked before binding"),
        },
    }
}

/// None uses scalar NULL placeholders for preparation-time schema validation.
/// No placeholder is ever executed or substituted into the retained template.
pub(super) fn bind_filter(
    code: &[ReadFilterOp],
    values: Option<&[GqlParameterValue]>,
) -> Result<Vec<GraphSetPredicateOp>, GraphSetTextError> {
    let operand = |value: &ReadFilterOperand| -> Result<GraphSetOperand, GraphSetTextError> {
        Ok(match value {
            ReadFilterOperand::Column(column) => GraphSetOperand::Column(*column),
            ReadFilterOperand::Literal(value) => GraphSetOperand::Literal(value.clone()),
            ReadFilterOperand::Parameter { index, at } => GraphSetOperand::Literal(match values {
                Some(values) => scalar(values[*index].clone(), *at)?,
                None => GqlScalarParameter::new(CanonicalScalar::Null)
                    .expect("canonical null is a bounded scalar"),
            }),
        })
    };
    code.iter()
        .map(|op| {
            Ok(match op {
                ReadFilterOp::Compare {
                    left,
                    comparison,
                    right,
                } => GraphSetPredicateOp::Compare {
                    left: operand(left)?,
                    comparison: *comparison,
                    right: operand(right)?,
                },
                ReadFilterOp::IsNull {
                    operand: value,
                    is_null,
                } => GraphSetPredicateOp::IsNull {
                    operand: operand(value)?,
                    is_null: *is_null,
                },
                ReadFilterOp::Truth(value) => GraphSetPredicateOp::Truth(*value),
                ReadFilterOp::Not => GraphSetPredicateOp::Not,
                ReadFilterOp::And => GraphSetPredicateOp::And,
                ReadFilterOp::Or => GraphSetPredicateOp::Or,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::{
        GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, PreparedGraphSetText,
    };
    use fgdb_types::CanonicalScalar;

    fn execute(statement: &str) -> Vec<GraphValueRow> {
        PreparedGraphSetText::prepare(statement, |_, _| None)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .execute_governed(
                GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || Ok::<_, usize>(()),
            )
            .unwrap()
            .value
    }

    fn row(value: i64) -> GraphValueRow {
        GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(value))])
    }

    #[test]
    fn with_where_pages_preserve_written_filter_boundaries() {
        assert_eq!(
            execute(
                "UNWIND [5, 1, 4, 2, 3] AS n WITH n WHERE n > 2 \
                 ORDER BY n DESC SKIP 1 LIMIT 1 RETURN n"
            ),
            vec![row(4)]
        );
        assert_eq!(
            execute(
                "UNWIND [3, 1, 2] AS n WITH n ORDER BY n LIMIT 1 \
                 WHERE n > 1 ORDER BY n DESC LIMIT 1 RETURN n"
            ),
            Vec::<GraphValueRow>::new()
        );
        assert_eq!(
            execute(
                "UNWIND [3, 1, 2] AS n WITH n WHERE n > 1 \
                 ORDER BY n LIMIT 1 RETURN n"
            ),
            vec![row(2)]
        );
    }

    #[test]
    fn filtered_pages_continue_into_unwind_and_renamed_with_stages() {
        assert_eq!(
            execute(
                "UNWIND [1, 2, 3] AS n WITH n WHERE n >= 2 ORDER BY n DESC LIMIT 1 \
                 UNWIND [n, n + 10] AS m WITH m AS value WHERE value > 3 \
                 ORDER BY value LIMIT 1 RETURN value"
            ),
            vec![row(13)]
        );
        assert_eq!(
            execute(
                "UNWIND [1, 2] AS n WITH n WHERE n > 1 \
                 UNWIND [n] AS value RETURN value"
            ),
            vec![row(2)]
        );
    }

    #[test]
    fn post_filter_pages_use_only_current_aliases_and_typed_parameters() {
        let prepared = PreparedGraphSetText::prepare(
            "UNWIND [3, 1] AS n WITH n AS kept WHERE kept > 0 \
             ORDER BY kept SKIP $skip LIMIT $count RETURN kept",
            |_, _| None,
        )
        .unwrap();
        let parameters = prepared.parameter_schema();
        assert_eq!(parameters.len(), 2);
        assert_eq!(parameters[0].name, "skip");
        assert_eq!(parameters[1].name, "count");
        assert!(
            parameters
                .iter()
                .all(|parameter| parameter.parameter_type == GqlParameterType::UInt64)
        );
        let statement = "UNWIND [1] AS n WITH n AS kept WHERE kept > 0 ORDER BY n RETURN kept";
        let error = PreparedGraphSetText::prepare(statement, |_, _| None).unwrap_err();
        assert_eq!(error.offset, statement.find("n RETURN").unwrap());
    }

    #[test]
    fn duplicate_filters_and_pages_require_a_new_stage() {
        for statement in [
            "UNWIND [1] AS n WITH n WHERE n > 0 WHERE n < 2 RETURN n",
            "UNWIND [1] AS n WITH n WHERE n > 0 ORDER BY n ORDER BY n RETURN n",
            "UNWIND [1] AS n WITH n WHERE n > 0 LIMIT 1 LIMIT 1 RETURN n",
        ] {
            assert!(PreparedGraphSetText::prepare(statement, |_, _| None).is_err());
        }
    }
}
