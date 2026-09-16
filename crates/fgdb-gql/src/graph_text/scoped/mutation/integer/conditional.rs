//! CASE syntax on the same scalar lexer and parameter table. Conditions lower
//! to typed Boolean instructions inside the integer program. No AST is retained
//! for execution, no branch becomes query text, and every explicit argument is
//! registered exactly once even when multiple WHEN clauses share a selector.

use super::*;
use crate::algebra::IntegerComparison;

fn bound(program: &mut Vec<ParsedOp>, op: GraphIntegerOp, at: usize) -> Result<(), GraphMutationTextError> {
    emit(program, ParsedOp::Bound(op), at)
}

impl<'a> Parser<'a> {
    /// Preserve a bound vertex/property named `case` when followed by its
    /// ordinary suffix. CASE syntax is selected by an actual following operand
    /// or WHEN, not by replacing every occurrence of a keyword-looking name.
    pub(in crate::graph_text) fn starts_integer_case(&self) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("CASE") { return Ok(false); }
        let next = self.lexer.clone().next()?;
        Ok(match next.kind {
            TokenKind::Word(word) => !["AS", "THEN", "ELSE", "END", "AND", "OR", "RETURN", "SET",
                "REMOVE", "GROUP", "HAVING", "ORDER", "SKIP", "LIMIT", "WITH", "WHERE"].iter()
                .any(|suffix| word.eq_ignore_ascii_case(suffix)),
            TokenKind::Digits(_) | TokenKind::Parameter(_) | TokenKind::Quoted(_)
            | TokenKind::Punct(b'(' | b'+' | b'-') => true,
            _ => false,
        })
    }

    pub(super) fn integer_case(
        &mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize,
        program: &mut Vec<ParsedOp>, at: usize,
    ) -> Result<(), GraphMutationTextError> {
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(at, GraphMutationTextErrorKind::IntegerNesting { limit: MAX_INTEGER_NESTING }));
        }
        let searched = self.is_word("WHEN");
        if !searched { self.integer_sum(columns, depth, program)?; }
        let mut alternatives = 0;
        loop {
            self.word("WHEN")?;
            if searched { self.case_disjunction(columns, depth, program)?; }
            else { self.integer_sum(columns, depth, program)?; }
            self.word("THEN")?;
            self.integer_sum(columns, depth, program)?;
            alternatives += 1;
            if !self.is_word("WHEN") { break; }
        }
        if self.take_word("ELSE")? { self.integer_sum(columns, depth, program)?; }
        else { bound(program, GraphIntegerOp::Literal(None), at)?; }
        self.word("END")?;
        if searched {
            // Postfix c1,t1,c2,t2,default,Case,Case compiles to ordered lazy
            // alternatives. No later condition executes once a branch matches.
            for _ in 0..alternatives { bound(program, GraphIntegerOp::Case, at)?; }
        } else {
            bound(program, GraphIntegerOp::SimpleCase { alternatives }, at)?;
        }
        Ok(())
    }

    fn case_disjunction(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        self.case_conjunction(columns, depth, program)?;
        while self.is_word("OR") {
            let at = self.current.at; self.advance()?;
            self.case_conjunction(columns, depth, program)?;
            bound(program, GraphIntegerOp::Or, at)?;
        }
        Ok(())
    }

    fn case_conjunction(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        self.case_negation(columns, depth, program)?;
        while self.is_word("AND") {
            let at = self.current.at; self.advance()?;
            self.case_negation(columns, depth, program)?;
            bound(program, GraphIntegerOp::And, at)?;
        }
        Ok(())
    }

    fn case_negation(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        let at = self.current.at;
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(at, GraphMutationTextErrorKind::IntegerNesting { limit: MAX_INTEGER_NESTING }));
        }
        if self.is_word("NOT") && !matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.')) {
            self.advance()?;
            self.case_negation(columns, depth + 1, program)?;
            return bound(program, GraphIntegerOp::Not, at);
        }
        if self.is_punct(b'(') && !self.case_parenthesis_is_numeric()? {
            self.advance()?;
            self.case_disjunction(columns, depth + 1, program)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let next = self.lexer.clone().next()?;
        let scalar_suffix = matches!(next.kind, TokenKind::Punct(b'.' | b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'!' | b'<' | b'>'))
            || matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IS"));
        if !scalar_suffix && (self.is_word("TRUE") || self.is_word("FALSE") || self.is_word("NULL")) {
            let value = if self.is_word("NULL") { None } else { Some(self.is_word("TRUE")) };
            self.advance()?;
            return bound(program, GraphIntegerOp::Truth(value), at);
        }
        self.integer_sum(columns, depth, program)?;
        if self.take_word("IS")? {
            let negate = self.take_word("NOT")?;
            self.word("NULL")?;
            return bound(program, GraphIntegerOp::IsNull(!negate), at);
        }
        let comparison: IntegerComparison = self.comparison()?;
        self.integer_sum(columns, depth, program)?;
        bound(program, GraphIntegerOp::Compare(comparison), at)
    }

    /// Only lexical lookahead: it never registers arguments, resolves symbols
    /// or retries a partially parsed condition. (x+1)>2 is arithmetic grouping;
    /// (x>2 OR y IS NULL) is Boolean grouping. Opaque quoted tokens and nested
    /// CASE expressions cannot inject a parenthesis into this decision.
    fn case_parenthesis_is_numeric(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut depth = 1;
        loop {
            let token = lexer.next()?;
            match token.kind {
                TokenKind::Punct(b'(') => {
                    depth += 1;
                    if depth > MAX_INTEGER_NESTING {
                        return Err(error(token.at, GraphPatternTextErrorKind::Expected("bounded CASE parentheses")));
                    }
                }
                TokenKind::Punct(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        let next = lexer.next()?;
                        return Ok(matches!(next.kind, TokenKind::Punct(b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'!' | b'<' | b'>'))
                            || matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IS")));
                    }
                }
                TokenKind::End => return Err(error(token.at, GraphPatternTextErrorKind::Expected("closing CASE parenthesis"))),
                _ => {}
            }
        }
    }
}
