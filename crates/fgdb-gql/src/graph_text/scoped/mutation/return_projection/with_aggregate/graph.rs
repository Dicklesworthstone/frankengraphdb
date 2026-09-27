//! A real graph-to-row input for the first aggregate WITH of a MATCH part.
//! The shared row aggregate parser consumes the original WITH, not rewritten
//! text. Property reads remain ordinary source projections, subject to the
//! same graph-source authorization and null-extension boundaries.

use super::*;

const HIDDEN_NAMES: [&str; 16] = [
    "__fg_group_0", "__fg_group_1", "__fg_group_2", "__fg_group_3",
    "__fg_group_4", "__fg_group_5", "__fg_group_6", "__fg_group_7",
    "__fg_group_8", "__fg_group_9", "__fg_group_10", "__fg_group_11",
    "__fg_group_12", "__fg_group_13", "__fg_group_14", "__fg_group_15",
];

fn scope_end(word: &str, previous: Option<TokenKind<'_>>) -> bool {
    if word.eq_ignore_ascii_case("WITH")
        && matches!(previous, Some(TokenKind::Word(word))
            if word.eq_ignore_ascii_case("STARTS") || word.eq_ignore_ascii_case("ENDS"))
    {
        return false;
    }
    ["WHERE", "ORDER", "SKIP", "LIMIT", "RETURN", "WITH", "MATCH",
        "OPTIONAL", "UNWIND", "UNION", "EXCEPT", "INTERSECT", "CALL"]
        .iter().any(|keyword| word.eq_ignore_ascii_case(keyword))
}

impl<'a> Parser<'a> {
    /// Leave WITH unconsumed and expose only native graph bindings plus the
    /// properties named by THIS grouping clause. The hidden fields are visible
    /// only as variable.property, never by their internal aliases or through *.
    /// Later clauses may use only actual grouping output aliases. In particular,
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
        let mut outputs: Vec<_> = incoming.iter().enumerate()
            .map(|(column, &(name, _))| (name, ReadValueTemplate::Column(column)))
            .collect();
        let bindings: Vec<_> = self.visible_graph_bindings().collect();
        for &variable in &bindings {
            if incoming.iter().any(|(name, _)| name.text == variable.text) {
                continue;
            }
            self.capacity(outputs.len(), MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns)?;
            let column = self.mutation_projection(&mut inputs, variable, None)?;
            outputs.push((variable, ReadValueTemplate::Column(width + column)));
        }
        let visible = outputs.len();
        let mut reads: Vec<(&'a str, Name<'a>, usize)> = Vec::new();
        let mut lexer = self.lexer.clone();
        let mut previous = None;
        let mut depth = 0_usize;
        loop {
            let token = lexer.next()?;
            let named = matches!(previous, Some(TokenKind::Punct(b'.')))
                || matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
            match token.kind {
                TokenKind::End => break,
                TokenKind::Word(word) if !named => {
                    if depth == 0 && scope_end(word, previous) {
                        break;
                    }
                    let mut lookahead = lexer.clone();
                    if matches!(lookahead.next()?.kind, TokenKind::Punct(b'.')) {
                        let property = lookahead.next()?;
                        if let TokenKind::Word(key) = property.kind {
                            if optional && incoming.iter().any(|(name, _)| name.text == word) {
                                return Err(expected(token.at,
                                    "project carried vertex properties before OPTIONAL MATCH"));
                            }
                            if let Some(&variable) = bindings.iter().find(|name| name.text == word) {
                                self.require_property_variable(variable)?;
                                if !reads.iter().any(|&(owner, property, _)| owner == word && property.text == key) {
                                    self.capacity(outputs.len(), MAX_PATTERN_VERTICES,
                                        crate::algebra::PatternLimitDimension::Columns)?;
                                    let name = HIDDEN_NAMES.iter().copied()
                                        .find(|name| outputs.iter().all(|(output, _)| output.text != *name))
                                        .ok_or_else(|| expected(token.at, "bounded grouping property reads"))?;
                                    let property = Name { text: key, at: property.at };
                                    let column = self.mutation_projection(&mut inputs, variable, Some(property))?;
                                    reads.push((word, property, outputs.len()));
                                    outputs.push((Name { text: name, at: token.at },
                                        ReadValueTemplate::Column(width + column)));
                                }
                            }
                        }
                    }
                }
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else { break; };
                    depth = outer;
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
        if outputs.is_empty() {
            // MATCH () still produces real occurrences. Project an existing
            // anonymous binding solely to carry those rows; it remains private.
            let variable = self.syntax.variables[0];
            let column = self.mutation_projection(&mut inputs, variable, None)?;
            outputs.push((Name { text: HIDDEN_NAMES[0], at },
                ReadValueTemplate::Column(width + column)));
        }
        if outputs.len() != visible {
            self.boundary_reads = Some(BoundaryReads { visible, width: outputs.len(), reads });
        }
        Ok(GraphProjectionHead { with: true, inputs, outputs })
    }
}

#[cfg(test)]
mod tests;
