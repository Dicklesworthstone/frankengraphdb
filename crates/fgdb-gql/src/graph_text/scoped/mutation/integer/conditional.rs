//! CASE syntax on the same scalar lexer and parameter table. Conditions lower
//! to typed Boolean instructions inside the integer program. No AST is retained
//! for execution, no branch becomes query text, and every explicit argument is
//! registered exactly once even when multiple WHEN clauses share a selector.

use super::*;

fn bound(
    program: &mut Vec<ParsedOp>,
    op: GraphIntegerOp,
    at: usize,
) -> Result<(), GraphMutationTextError> {
    emit(program, ParsedOp::Bound(op), at)
}

impl<'a> Parser<'a> {
    /// Preserve a bound vertex/property named `case` when followed by its
    /// ordinary suffix. CASE syntax is selected by an actual following operand
    /// or WHEN, not by replacing every occurrence of a keyword-looking name.
    pub(in crate::graph_text) fn starts_integer_case(&self) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("CASE") {
            return Ok(false);
        }
        let next = self.lexer.clone().next()?;
        Ok(match next.kind {
            TokenKind::Word(word) => ![
                "AS", "THEN", "ELSE", "END", "AND", "OR", "RETURN", "SET", "REMOVE", "GROUP",
                "HAVING", "ORDER", "SKIP", "OFFSET", "LIMIT", "WITH", "WHERE",
            ]
            .iter()
            .any(|suffix| word.eq_ignore_ascii_case(suffix)),
            TokenKind::Digits(_)
            | TokenKind::Parameter(_)
            | TokenKind::Quoted(_)
            | TokenKind::Punct(b'(' | b'+' | b'-') => true,
            _ => false,
        })
    }

    pub(super) fn integer_case(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<(), GraphMutationTextError> {
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(
                at,
                GraphMutationTextErrorKind::IntegerNesting {
                    limit: MAX_INTEGER_NESTING,
                },
            ));
        }
        // Every position is a whole scalar expression, as in openCypher: a
        // searched condition is any Boolean (`WHEN f`, `WHEN x IN [..]`,
        // `WHEN NOT n.p STARTS WITH 'a'`), and a result or simple-CASE
        // operand may itself compare. The compiler types each condition.
        let searched = self.is_word("WHEN");
        if !searched {
            self.scalar_boolean(columns, depth, program)?;
        }
        let mut alternatives = 0;
        loop {
            self.word("WHEN")?;
            if searched {
                self.boolean_operand_at = self.current.at;
            }
            self.scalar_boolean(columns, depth, program)?;
            self.word("THEN")?;
            self.scalar_boolean(columns, depth, program)?;
            alternatives += 1;
            if !self.is_word("WHEN") {
                break;
            }
        }
        if self.take_word("ELSE")? {
            self.scalar_boolean(columns, depth, program)?;
        } else {
            bound(program, GraphIntegerOp::Literal(None), at)?;
        }
        self.word("END")?;
        if searched {
            // Postfix c1,t1,c2,t2,default,Case,Case compiles to ordered lazy
            // alternatives. No later condition executes once a branch matches.
            for _ in 0..alternatives {
                bound(program, GraphIntegerOp::Case, at)?;
            }
        } else {
            bound(program, GraphIntegerOp::SimpleCase { alternatives }, at)?;
        }
        Ok(())
    }
}
