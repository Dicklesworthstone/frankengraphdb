//! A real graph-to-row input for the first aggregate WITH of a MATCH part,
//! or the implied WITH * before its aggregate RETURN.
//! The shared row aggregate parser consumes the original clause, not rewritten
//! text. Property reads remain ordinary source projections, subject to the
//! same graph-source authorization and null-extension boundaries.

use super::*;

pub(super) const HIDDEN_NAMES: [&str; 16] = [
    "__fg_group_0",
    "__fg_group_1",
    "__fg_group_2",
    "__fg_group_3",
    "__fg_group_4",
    "__fg_group_5",
    "__fg_group_6",
    "__fg_group_7",
    "__fg_group_8",
    "__fg_group_9",
    "__fg_group_10",
    "__fg_group_11",
    "__fg_group_12",
    "__fg_group_13",
    "__fg_group_14",
    "__fg_group_15",
];

fn scope_end(word: &str, previous: Option<TokenKind<'_>>) -> bool {
    if word.eq_ignore_ascii_case("WITH")
        && matches!(previous, Some(TokenKind::Word(word))
            if word.eq_ignore_ascii_case("STARTS") || word.eq_ignore_ascii_case("ENDS"))
    {
        return false;
    }
    [
        "WHERE",
        "ORDER",
        "SKIP",
        "OFFSET",
        "LIMIT",
        "RETURN",
        "WITH",
        "MATCH",
        "OPTIONAL",
        "UNWIND",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "CALL",
    ]
    .iter()
    .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

impl<'a> Parser<'a> {
    /// Leave WITH/RETURN unconsumed and expose only native graph bindings plus the
    /// properties named by THIS grouping clause, or by the grouping's own
    /// WHERE, pages and RETURN through a binding it keeps as a bare key
    /// (`grouped_reads`). The hidden fields are visible only as
    /// variable.property, never by their internal aliases or through *. Later
    /// stages may use only actual grouping output aliases. In particular,
    /// properties of an imported OPTIONAL binding cannot read its nullable copy.
    pub(in crate::graph_text::scoped::mutation::return_projection) fn with_graph_head(
        &mut self,
        incoming: &[(Name<'a>, GraphSetColumnType)],
        mut inputs: Vec<Projection<'a>>,
        optional: bool,
    ) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        let at = self.current.at;
        let width = incoming.len();
        self.syntax.distinct = false;
        self.boundary_reads = None;
        let mut outputs: Vec<_> = incoming
            .iter()
            .enumerate()
            .map(|(column, &(name, _))| (name, ReadValueTemplate::Column(column)))
            .collect();
        let bindings: Vec<_> = self.visible_graph_bindings().collect();
        for &variable in &bindings {
            if incoming.iter().any(|(name, _)| name.text == variable.text) {
                continue;
            }
            self.capacity(
                outputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let column = self.mutation_projection(&mut inputs, variable, None)?;
            outputs.push((variable, ReadValueTemplate::Column(width + column)));
        }
        let visible = outputs.len();
        let mut reads: Vec<(&'a str, Name<'a>, usize)> = Vec::new();
        let mut functions: Vec<(&'a str, GraphPathFunction, usize)> = Vec::new();
        // Items that keep a binding as a bare key (`p` or `p AS q`), as
        // (output alias, binding). `item` holds an item's first tokens.
        let mut kept: Vec<(&'a str, &'a str)> = Vec::new();
        let mut item: [Option<TokenKind<'a>>; 3] = [None; 3];
        let mut item_len = 0_usize;
        let mut lexer = self.lexer.clone();
        let mut previous = None;
        let mut depth = 0_usize;
        let mut end = None;
        // The pattern comprehensions this head gathers (hoist_comprehension),
        // the end of the one the scan is stepping over, and the element
        // locals bound so far. Under OPTIONAL MATCH an imported binding is
        // the nullable re-matched copy, so no comprehension may read it.
        let mut comprehensions = Vec::new();
        let mut skip_until = 0_usize;
        let mut locals: Vec<&'a str> = Vec::new();
        let imported: Vec<&'a str> = if optional {
            incoming.iter().map(|(name, _)| name.text).collect()
        } else {
            Vec::new()
        };
        loop {
            let token = lexer.next()?;
            if token.at < skip_until {
                continue;
            }
            if let TokenKind::Word(word) = token.kind
                && pipeline::binds_local(previous, lexer.clone().next()?.kind)
            {
                locals.push(word);
            }
            if pipeline::hoist_candidate(
                token.kind,
                matches!(previous, Some(TokenKind::Punct(b'.'))),
            ) && let Some(span) = self.hoist_comprehension(
                &lexer,
                token,
                &mut outputs,
                &mut inputs,
                width,
                pipeline::HoistGuard {
                    names: &HIDDEN_NAMES,
                    locals: &locals,
                    refused: &imported,
                },
            )? {
                skip_until = span.1;
                comprehensions.push(span);
                if let Some(slot) = item.get_mut(item_len) {
                    *slot = Some(token.kind);
                }
                item_len += 1;
                previous = Some(TokenKind::Punct(b']'));
                continue;
            }
            let named = matches!(previous, Some(TokenKind::Punct(b'.')))
                || matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
            match token.kind {
                TokenKind::End => break,
                TokenKind::Word(word) if !named => {
                    if depth == 0 && scope_end(word, previous) {
                        end = Some(word);
                        break;
                    }
                    if let Some(source) = pipeline::property_map_source(word, token.at, &lexer)? {
                        if optional && incoming.iter().any(|(name, _)| name.text == source.text) {
                            return Err(expected(
                                source.at,
                                "project carried element maps before OPTIONAL MATCH",
                            ));
                        }
                        if let Some(&variable) =
                            bindings.iter().find(|name| name.text == source.text)
                            && !reads
                                .iter()
                                .any(|&(owner, key, _)| owner == source.text && key.text == "*")
                        {
                            let column = self.hidden_property_map(
                                &mut outputs,
                                &mut inputs,
                                width,
                                variable,
                                source.at,
                            )?;
                            reads.push((
                                source.text,
                                Name {
                                    text: "*",
                                    at: source.at,
                                },
                                column,
                            ));
                        }
                    }
                    // `length(p)`, `nodes(p)`, `relationships(p)`, `labels(n)`
                    // or `type(r)` in a key or an aggregate argument reads the
                    // graph source once, as a hidden head column.
                    if let Some((source, function)) =
                        pipeline::metadata_source(word, token.at, &lexer)?
                        && let Some(variable) =
                            self.grouping_metadata_binding(&bindings, source.text, function)
                        && !functions
                            .iter()
                            .any(|&(owner, known, _)| owner == source.text && known == function)
                    {
                        if optional && incoming.iter().any(|(name, _)| name.text == source.text) {
                            return Err(expected(
                                source.at,
                                "project carried graph functions before OPTIONAL MATCH",
                            ));
                        }
                        let column = self.hidden_function(
                            &mut outputs,
                            &mut inputs,
                            width,
                            (variable, function),
                            source.at,
                        )?;
                        functions.push((source.text, function, column));
                    }
                    let mut lookahead = lexer.clone();
                    if matches!(lookahead.next()?.kind, TokenKind::Punct(b'.')) {
                        let property = lookahead.next()?;
                        if let TokenKind::Word(key) | TokenKind::DelimitedKeyword(key) =
                            property.kind
                        {
                            if let Some(column) =
                                incoming.iter().position(|(name, _)| name.text == word)
                                && !reads.iter().any(|&(owner, property, _)| {
                                    owner == word && property.text == key
                                })
                            {
                                // Grouping's ordinary row parser addresses
                                // hidden property cells. Compute this one from
                                // the preserved LEFT identity, never from a
                                // nullable re-matched RIGHT binding.
                                self.capacity(
                                    outputs.len(),
                                    MAX_PATTERN_VERTICES,
                                    crate::algebra::PatternLimitDimension::Columns,
                                )?;
                                let name = HIDDEN_NAMES
                                    .iter()
                                    .copied()
                                    .find(|name| {
                                        outputs.iter().all(|(output, _)| output.text != *name)
                                    })
                                    .ok_or_else(|| {
                                        expected(token.at, "bounded grouping property reads")
                                    })?;
                                let value = self.map_get(ReadValueTemplate::Column(column), key);
                                outputs.push((
                                    Name {
                                        text: name,
                                        at: token.at,
                                    },
                                    value,
                                ));
                                reads.push((
                                    word,
                                    Name {
                                        text: key,
                                        at: property.at,
                                    },
                                    outputs.len() - 1,
                                ));
                            }
                            if let Some(&variable) = bindings.iter().find(|name| name.text == word)
                                && !incoming.iter().any(|(name, _)| name.text == word)
                                && !reads.iter().any(|&(owner, property, _)| {
                                    owner == word && property.text == key
                                })
                            {
                                let property = Name {
                                    text: key,
                                    at: property.at,
                                };
                                let column = self.hidden_property(
                                    &mut outputs,
                                    &mut inputs,
                                    width,
                                    (variable, property),
                                    token.at,
                                )?;
                                reads.push((word, property, column));
                            }
                        }
                    }
                }
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        break;
                    };
                    depth = outer;
                }
                _ => {}
            }
            if depth == 0 && matches!(token.kind, TokenKind::Punct(b',')) {
                kept.extend(bare_key(&item, item_len));
                item_len = 0;
            } else if !(previous.is_none()
                && matches!(token.kind, TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("DISTINCT") || word.eq_ignore_ascii_case("ALL")))
            {
                if let Some(slot) = item.get_mut(item_len) {
                    *slot = Some(token.kind);
                }
                item_len += 1;
            }
            previous = Some(token.kind);
        }
        kept.extend(bare_key(&item, item_len));
        // A kept binding must be this part's own graph binding. An imported
        // row column of the same name is not a graph read.
        kept.retain(|&(_, binding)| {
            bindings.iter().any(|name| name.text == binding)
                && !incoming.iter().any(|(name, _)| name.text == binding)
        });
        let grouped = match end {
            Some(word) if !kept.is_empty() => self.grouped_reads(
                lexer,
                word,
                &kept,
                &bindings,
                &mut outputs,
                &mut inputs,
                width,
            )?,
            _ => Vec::new(),
        };
        if outputs.is_empty() {
            // MATCH () still produces real occurrences. Project an existing
            // anonymous binding solely to carry those rows; it remains private.
            let variable = self.syntax.variables[0];
            let column = self.mutation_projection(&mut inputs, variable, None)?;
            outputs.push((
                Name {
                    text: HIDDEN_NAMES[0],
                    at,
                },
                ReadValueTemplate::Column(width + column),
            ));
        }
        if outputs.len() != visible {
            self.boundary_reads = Some(BoundaryReads {
                visible,
                width: outputs.len(),
                reads,
                functions,
                grouped,
                scoped: Vec::new(),
            });
        }
        self.comprehension_reads =
            (!comprehensions.is_empty()).then_some((outputs.len(), comprehensions));
        Ok(GraphProjectionHead {
            with: true,
            plain: false,
            inputs,
            outputs,
        })
    }

    /// The hidden head column reading `variable.property`, appended after the
    /// visible outputs (`width` places graph inputs after incoming columns).
    /// `at` is where the read's variable is spelled.
    fn hidden_property(
        &mut self,
        outputs: &mut Vec<(Name<'a>, ReadValueTemplate)>,
        inputs: &mut Vec<Projection<'a>>,
        width: usize,
        (variable, property): (Name<'a>, Name<'a>),
        at: usize,
    ) -> Result<usize, GraphSetTextError> {
        self.require_property_variable(variable)?;
        self.capacity(
            outputs.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let name = HIDDEN_NAMES
            .iter()
            .copied()
            .find(|name| outputs.iter().all(|(output, _)| output.text != *name))
            .ok_or_else(|| expected(at, "bounded grouping property reads"))?;
        let column = self.mutation_projection(inputs, variable, Some(property))?;
        outputs.push((
            Name { text: name, at },
            ReadValueTemplate::Column(width + column),
        ));
        Ok(outputs.len() - 1)
    }

    /// The binding a grouping metadata read applies to, only where its domain
    /// is certain without consuming the call: labels() of a root vertex,
    /// type() of one root relationship, and length(), nodes() or
    /// relationships() of the named path. Anything else is left to the
    /// ordinary item compiler, which refuses it as before.
    fn grouping_metadata_binding(
        &self,
        bindings: &[Name<'a>],
        name: &str,
        function: GraphPathFunction,
    ) -> Option<Name<'a>> {
        let binding = bindings
            .iter()
            .copied()
            .find(|binding| binding.text == name)?;
        let admitted = match function {
            GraphPathFunction::Labels => self.syntax.variables[..self.syntax.root_variables]
                .iter()
                .any(|variable| variable.text == name),
            GraphPathFunction::Type => self
                .syntax
                .visible_edge(name)
                .is_some_and(|(edge, scoped)| !scoped && edge.walk.is_none()),
            GraphPathFunction::Length | GraphPathFunction::Nodes | GraphPathFunction::Edges => {
                self.syntax.names_path(name)
            }
            _ => false,
        };
        admitted.then_some(binding)
    }

    /// The hidden head column reading one metadata function of `variable`.
    fn hidden_function(
        &mut self,
        outputs: &mut Vec<(Name<'a>, ReadValueTemplate)>,
        inputs: &mut Vec<Projection<'a>>,
        width: usize,
        (variable, function): (Name<'a>, GraphPathFunction),
        at: usize,
    ) -> Result<usize, GraphSetTextError> {
        self.capacity(
            outputs.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let name = HIDDEN_NAMES
            .iter()
            .copied()
            .find(|name| outputs.iter().all(|(output, _)| output.text != *name))
            .ok_or_else(|| expected(at, "bounded grouping function reads"))?;
        let column = self.projection_slot(
            inputs,
            Projection {
                variable,
                property: None,
                path: Some(function),
                collect: None,
            },
        )?;
        outputs.push((
            Name { text: name, at },
            ReadValueTemplate::Column(width + column),
        ));
        Ok(outputs.len() - 1)
    }

    fn hidden_property_map(
        &mut self,
        outputs: &mut Vec<(Name<'a>, ReadValueTemplate)>,
        inputs: &mut Vec<Projection<'a>>,
        width: usize,
        variable: Name<'a>,
        at: usize,
    ) -> Result<usize, GraphSetTextError> {
        self.capacity(
            outputs.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let name = HIDDEN_NAMES
            .iter()
            .copied()
            .find(|name| outputs.iter().all(|(output, _)| output.text != *name))
            .ok_or_else(|| expected(at, "bounded grouping property-map reads"))?;
        let column = self.property_map_projection(inputs, variable)?;
        outputs.push((
            Name { text: name, at },
            ReadValueTemplate::Column(width + column),
        ));
        Ok(outputs.len() - 1)
    }

    /// `WITH p, count(f) AS c WHERE c > 1 RETURN p.name, c` (fgdb-ezgeq): the
    /// scope after the grouping (its WHERE, pages and the terminal RETURN's
    /// items) may read a property of a binding the WITH keeps as a bare key.
    /// Each read becomes a hidden head column here; the grouping stage keys it
    /// too. `lexer` continues after `end`, the word that closed the WITH's
    /// items. A read in a later WITH, UNWIND or MATCH part stays refused, as
    /// does one through any other alias.
    #[allow(clippy::too_many_arguments)]
    fn grouped_reads(
        &mut self,
        mut lexer: Lexer<'a>,
        end: &'a str,
        kept: &[(&'a str, &'a str)],
        bindings: &[Name<'a>],
        outputs: &mut Vec<(Name<'a>, ReadValueTemplate)>,
        inputs: &mut Vec<Projection<'a>>,
        width: usize,
    ) -> Result<Vec<(&'a str, &'a str, Name<'a>, usize)>, GraphSetTextError> {
        let mut grouped: Vec<(&'a str, &'a str, Name<'a>, usize)> = Vec::new();
        if !["WHERE", "ORDER", "SKIP", "OFFSET", "LIMIT", "RETURN"]
            .iter()
            .any(|keyword| end.eq_ignore_ascii_case(keyword))
        {
            return Ok(grouped);
        }
        let mut returned = end.eq_ignore_ascii_case("RETURN");
        let mut previous = Some(TokenKind::Word(end));
        let mut depth = 0_usize;
        loop {
            let token = lexer.next()?;
            let named = matches!(previous, Some(TokenKind::Punct(b'.')))
                || matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
            match token.kind {
                TokenKind::End => break,
                TokenKind::Word(word) if !named => {
                    if depth == 0 {
                        // Pages after RETURN belong to the set parser, which
                        // addresses the RETURN's own columns.
                        let page = ["ORDER", "SKIP", "OFFSET", "LIMIT"]
                            .iter()
                            .any(|keyword| word.eq_ignore_ascii_case(keyword));
                        if (returned && page)
                            || (!page
                                && !word.eq_ignore_ascii_case("WHERE")
                                && !word.eq_ignore_ascii_case("RETURN")
                                && scope_end(word, previous))
                        {
                            break;
                        }
                        returned |= word.eq_ignore_ascii_case("RETURN");
                    }
                    if let Some(source) = pipeline::property_map_source(word, token.at, &lexer)?
                        && let Some(&(alias, binding)) =
                            kept.iter().find(|(alias, _)| *alias == source.text)
                        && !grouped
                            .iter()
                            .any(|&(owner, _, key, _)| owner == alias && key.text == "*")
                        && let Some(&variable) = bindings.iter().find(|name| name.text == binding)
                    {
                        let column = match grouped
                            .iter()
                            .find(|&&(_, owner, key, _)| owner == binding && key.text == "*")
                        {
                            Some(&(_, _, _, column)) => column,
                            None => self
                                .hidden_property_map(outputs, inputs, width, variable, source.at)?,
                        };
                        grouped.push((
                            alias,
                            binding,
                            Name {
                                text: "*",
                                at: source.at,
                            },
                            column,
                        ));
                    }
                    let mut lookahead = lexer.clone();
                    if let Some(&(alias, binding)) = kept.iter().find(|(alias, _)| *alias == word)
                        && matches!(lookahead.next()?.kind, TokenKind::Punct(b'.'))
                    {
                        let property = lookahead.next()?;
                        if let TokenKind::Word(key) | TokenKind::DelimitedKeyword(key) =
                            property.kind
                            && !grouped
                                .iter()
                                .any(|&(owner, _, read, _)| owner == alias && read.text == key)
                            && let Some(&variable) =
                                bindings.iter().find(|name| name.text == binding)
                        {
                            let property = Name {
                                text: key,
                                at: property.at,
                            };
                            // The same binding may be kept under two aliases.
                            let column = match grouped
                                .iter()
                                .find(|&&(_, owner, read, _)| owner == binding && read.text == key)
                            {
                                Some(&(_, _, _, column)) => column,
                                None => self.hidden_property(
                                    outputs,
                                    inputs,
                                    width,
                                    (variable, property),
                                    token.at,
                                )?,
                            };
                            grouped.push((alias, binding, property, column));
                        }
                    }
                }
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        break;
                    };
                    depth = outer;
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
        Ok(grouped)
    }
}

/// `(alias, binding)` when a WITH item is exactly `binding` or `binding AS
/// alias`; `item` holds its first tokens and `len` counts all of them.
fn bare_key<'a>(item: &[Option<TokenKind<'a>>; 3], len: usize) -> Option<(&'a str, &'a str)> {
    match (len, item) {
        (1, [Some(TokenKind::Word(binding)), ..]) => Some((binding, binding)),
        (
            3,
            [
                Some(TokenKind::Word(binding)),
                Some(TokenKind::Word(as_)),
                Some(TokenKind::Word(alias)),
            ],
        ) if as_.eq_ignore_ascii_case("AS") => Some((alias, binding)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
