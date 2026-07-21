//! Activation-predicate language for invariant clauses.
//!
//! A clause's `activation_predicate` is a boolean expression over the atoms
//! a capability manifest enables (features ∪ postures ∪ roles):
//!
//!   expr    := or
//!   or      := and ( "||" and )*
//!   and     := unary ( "&&" unary )*
//!   unary   := "!" unary | primary
//!   primary := "(" expr ")" | "true" | "false" | IDENT
//!   IDENT   := [a-z0-9_-]+  (also '.' and ':' for namespaced atoms)
//!
//! An atom evaluates true iff it is present in the manifest. Unknown atoms
//! evaluate false (a clause guarded by an unlanded feature is unreachable,
//! exactly as the activation contract intends).

use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    True,
    False,
    Atom(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PredicateError {
    pub msg: String,
}

impl fmt::Display for PredicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for PredicateError {}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

fn is_atom_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.' | ':')
}

impl Parser {
    fn skip_ws(&mut self) {
        while matches!(self.chars.get(self.pos), Some(' ') | Some('\t')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn err(&self, msg: impl Into<String>) -> PredicateError {
        PredicateError { msg: msg.into() }
    }

    fn parse_or(&mut self) -> Result<Expr, PredicateError> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') {
                if self.chars.get(self.pos + 1) != Some(&'|') {
                    return Err(self.err("single '|' (expected '||')"));
                }
                self.pos += 2;
                let right = self.parse_and()?;
                left = Expr::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> Result<Expr, PredicateError> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('&') {
                if self.chars.get(self.pos + 1) != Some(&'&') {
                    return Err(self.err("single '&' (expected '&&')"));
                }
                self.pos += 2;
                let right = self.parse_unary()?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, PredicateError> {
        self.skip_ws();
        if self.peek() == Some('!') {
            self.pos += 1;
            Ok(Expr::Not(Box::new(self.parse_unary()?)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, PredicateError> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let inner = self.parse_or()?;
                self.skip_ws();
                if self.peek() != Some(')') {
                    return Err(self.err("expected ')'"));
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(c) if is_atom_char(c) => {
                let start = self.pos;
                while self.peek().is_some_and(is_atom_char) {
                    self.pos += 1;
                }
                let word: String = self.chars[start..self.pos].iter().collect();
                match word.as_str() {
                    "true" => Ok(Expr::True),
                    "false" => Ok(Expr::False),
                    _ => Ok(Expr::Atom(word)),
                }
            }
            Some(c) => Err(self.err(format!("unexpected character {c:?}"))),
            None => Err(self.err("unexpected end of predicate")),
        }
    }
}

/// Parse an activation predicate.
pub fn parse(text: &str) -> Result<Expr, PredicateError> {
    let mut p = Parser {
        chars: text.chars().collect(),
        pos: 0,
    };
    let expr = p.parse_or()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(PredicateError {
            msg: format!("trailing input at offset {}", p.pos),
        });
    }
    Ok(expr)
}

/// Evaluate under the set of enabled atoms.
pub fn eval(expr: &Expr, enabled: &BTreeSet<String>) -> bool {
    match expr {
        Expr::True => true,
        Expr::False => false,
        Expr::Atom(a) => enabled.contains(a),
        Expr::Not(e) => !eval(e, enabled),
        Expr::And(a, b) => eval(a, enabled) && eval(b, enabled),
        Expr::Or(a, b) => eval(a, enabled) || eval(b, enabled),
    }
}

/// Collect the atoms a predicate mentions (capability attribution for the
/// closure report: which capability is behind an absent clause).
pub fn atoms(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::True | Expr::False => {}
        Expr::Atom(a) => {
            out.insert(a.clone());
        }
        Expr::Not(e) => atoms(e, out),
        Expr::And(a, b) | Expr::Or(a, b) => {
            atoms(a, out);
            atoms(b, out);
        }
    }
}
