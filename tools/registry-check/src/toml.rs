//! Minimal in-house TOML-subset parser for the G0 claim registries.
//!
//! The closed dependency universe (FG-CON-01) applies to the tooling that
//! enforces it, so this parser is std-only and deliberately small. It covers
//! exactly the subset the registries are authored in:
//!
//!   - comments (`#` to end of line)
//!   - `[table.path]` and `[[array.of.tables.path]]` headers
//!   - `key = value` with bare or quoted keys
//!   - values: basic strings (with escapes), literal strings, multi-line
//!     basic (`"""`) and literal (`'''`) strings, integers, booleans, arrays
//!
//! Everything outside the subset (floats, dates, inline tables, dotted keys
//! in key position, line-continuation backslashes) is a *typed* error, never
//! a panic: mutated registry bytes must fail closed (`claims_registry_toml_fuzz`).

use std::collections::BTreeMap;
use std::fmt;

/// A parsed TOML value (registry subset).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<Value>),
    Table(Table),
}

pub type Table = BTreeMap<String, Value>;

/// Typed parse error with 1-based line position.
#[derive(Debug, Clone, PartialEq)]
pub struct TomlError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for TomlError {}

struct Scanner {
    chars: Vec<char>,
    pos: usize,
    line: usize,
}

impl Scanner {
    fn new(text: &str) -> Self {
        Scanner {
            chars: text.chars().collect(),
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if let Some(ch) = c {
            self.pos += 1;
            if ch == '\n' {
                self.line += 1;
            }
        }
        c
    }

    fn err(&self, msg: impl Into<String>) -> TomlError {
        TomlError {
            line: self.line,
            msg: msg.into(),
        }
    }

    /// Skip spaces and tabs (not newlines).
    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.bump();
        }
    }

    /// Skip whitespace, comments, and newlines.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\n') | Some('\r') => {
                    self.bump();
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    /// Consume to end of line, allowing only whitespace and a comment.
    fn expect_eol(&mut self) -> Result<(), TomlError> {
        self.skip_inline_ws();
        match self.peek() {
            None => Ok(()),
            Some('#') => {
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
                Ok(())
            }
            Some('\n') => {
                self.bump();
                Ok(())
            }
            Some('\r') => {
                self.bump();
                if self.peek() == Some('\n') {
                    self.bump();
                    Ok(())
                } else {
                    Err(self.err("bare carriage return"))
                }
            }
            Some(c) => Err(self.err(format!("unexpected character {c:?} after value"))),
        }
    }

    fn parse_bare_key(&mut self) -> Result<String, TomlError> {
        let mut key = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                key.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if key.is_empty() {
            Err(self.err("expected key"))
        } else {
            Ok(key)
        }
    }

    fn parse_key(&mut self) -> Result<String, TomlError> {
        match self.peek() {
            Some('"') => {
                self.bump();
                self.parse_basic_string_body('"')
            }
            Some('\'') => {
                self.bump();
                self.parse_literal_string_body('\'')
            }
            _ => self.parse_bare_key(),
        }
    }

    /// Body of a single-line basic string; opening quote already consumed.
    fn parse_basic_string_body(&mut self, delim: char) -> Result<String, TomlError> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some('\n') => return Err(self.err("newline in single-line string")),
                Some(c) if c == delim => return Ok(out),
                Some('\\') => out.push(self.parse_escape()?),
                Some(c) => out.push(c),
            }
        }
    }

    fn parse_escape(&mut self) -> Result<char, TomlError> {
        match self.bump() {
            None => Err(self.err("unterminated escape")),
            Some('"') => Ok('"'),
            Some('\\') => Ok('\\'),
            Some('b') => Ok('\u{0008}'),
            Some('f') => Ok('\u{000C}'),
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('t') => Ok('\t'),
            Some('u') => self.parse_unicode_escape(4),
            Some('U') => self.parse_unicode_escape(8),
            Some('\n') => {
                Err(self.err("line-continuation backslash is outside the registry subset"))
            }
            Some(c) => Err(self.err(format!("unknown escape \\{c}"))),
        }
    }

    fn parse_unicode_escape(&mut self, len: usize) -> Result<char, TomlError> {
        let mut v: u32 = 0;
        for _ in 0..len {
            let c = self
                .bump()
                .ok_or_else(|| self.err("unterminated unicode escape"))?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| self.err(format!("invalid hex digit {c:?} in unicode escape")))?;
            v = v
                .checked_mul(16)
                .and_then(|x| x.checked_add(d))
                .ok_or_else(|| self.err("unicode escape overflow"))?;
        }
        char::from_u32(v).ok_or_else(|| self.err(format!("invalid unicode scalar U+{v:X}")))
    }

    /// Body of a single-line literal string; opening quote already consumed.
    fn parse_literal_string_body(&mut self, delim: char) -> Result<String, TomlError> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated literal string")),
                Some('\n') => return Err(self.err("newline in single-line literal string")),
                Some(c) if c == delim => return Ok(out),
                Some(c) => out.push(c),
            }
        }
    }

    /// Multi-line string; the three opening quotes already consumed.
    /// `basic` selects escape processing (""") vs raw (''').
    fn parse_multiline_string(&mut self, delim: char, basic: bool) -> Result<String, TomlError> {
        // A newline immediately after the opening delimiter is trimmed.
        if self.peek() == Some('\r') && self.peek_at(1) == Some('\n') {
            self.bump();
            self.bump();
        } else if self.peek() == Some('\n') {
            self.bump();
        }
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated multi-line string")),
                Some(c) if c == delim => {
                    // Count consecutive delimiter characters.
                    let mut q = 0;
                    while self.peek() == Some(delim) {
                        self.bump();
                        q += 1;
                        if q == 5 {
                            break;
                        }
                    }
                    if q >= 3 {
                        // Up to two delimiter chars may be content just
                        // before the closing triple.
                        for _ in 0..(q - 3) {
                            out.push(delim);
                        }
                        return Ok(out);
                    }
                    for _ in 0..q {
                        out.push(delim);
                    }
                }
                Some('\\') if basic => {
                    self.bump();
                    out.push(self.parse_escape()?);
                }
                Some(_) => {
                    // Safe: peek() returned Some.
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, TomlError> {
        match self.peek() {
            None => Err(self.err("expected value")),
            Some('"') => {
                if self.peek_at(1) == Some('"') && self.peek_at(2) == Some('"') {
                    self.bump();
                    self.bump();
                    self.bump();
                    Ok(Value::Str(self.parse_multiline_string('"', true)?))
                } else {
                    self.bump();
                    Ok(Value::Str(self.parse_basic_string_body('"')?))
                }
            }
            Some('\'') => {
                if self.peek_at(1) == Some('\'') && self.peek_at(2) == Some('\'') {
                    self.bump();
                    self.bump();
                    self.bump();
                    Ok(Value::Str(self.parse_multiline_string('\'', false)?))
                } else {
                    self.bump();
                    Ok(Value::Str(self.parse_literal_string_body('\'')?))
                }
            }
            Some('[') => {
                self.bump();
                self.parse_array()
            }
            Some('{') => Err(self.err("inline tables are outside the registry subset")),
            Some(c) if c == 't' || c == 'f' => self.parse_bool(),
            Some(c) if c.is_ascii_digit() || c == '+' || c == '-' => self.parse_int(),
            Some(c) => Err(self.err(format!("unexpected character {c:?} at start of value"))),
        }
    }

    fn parse_bool(&mut self) -> Result<Value, TomlError> {
        let word = self.parse_bare_key()?;
        match word.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            other => Err(self.err(format!("expected boolean, found {other:?}"))),
        }
    }

    fn parse_int(&mut self) -> Result<Value, TomlError> {
        let mut digits = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-' || c == '.' || c == ':'
            {
                digits.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if digits.contains('.') || digits.contains('e') || digits.contains('E') {
            return Err(self.err(format!(
                "floats are outside the registry subset (token {digits:?})"
            )));
        }
        if digits.contains(':') || digits.rmatch_indices('-').any(|(i, _)| i > 0) {
            return Err(self.err(format!(
                "dates/times are outside the registry subset (token {digits:?})"
            )));
        }
        let normalized: String = digits.chars().filter(|&c| c != '_').collect();
        normalized
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| self.err(format!("invalid integer token {digits:?}")))
    }

    /// Array; opening bracket already consumed. Newlines and comments are
    /// allowed inside; trailing comma is allowed.
    fn parse_array(&mut self) -> Result<Value, TomlError> {
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(self.err("unterminated array")),
                Some(']') => {
                    self.bump();
                    return Ok(Value::Array(items));
                }
                _ => {
                    items.push(self.parse_value()?);
                    self.skip_trivia();
                    match self.peek() {
                        Some(',') => {
                            self.bump();
                        }
                        Some(']') => {
                            self.bump();
                            return Ok(Value::Array(items));
                        }
                        None => return Err(self.err("unterminated array")),
                        Some(c) => {
                            return Err(
                                self.err(format!("expected ',' or ']' in array, found {c:?}"))
                            );
                        }
                    }
                }
            }
        }
    }

    fn parse_header_path(&mut self) -> Result<Vec<String>, TomlError> {
        let mut path = Vec::new();
        loop {
            self.skip_inline_ws();
            path.push(self.parse_key()?);
            self.skip_inline_ws();
            match self.peek() {
                Some('.') => {
                    self.bump();
                }
                _ => return Ok(path),
            }
        }
    }
}

/// Navigate to (or create) the table a header path addresses.
///
/// Standard TOML semantics for the subset: intermediate segments descend into
/// tables, or into the *last* element of an array-of-tables.
fn navigate<'a>(
    root: &'a mut Table,
    path: &[String],
    line: usize,
) -> Result<&'a mut Table, TomlError> {
    let mut current = root;
    for seg in path {
        let entry = current
            .entry(seg.clone())
            .or_insert_with(|| Value::Table(Table::new()));
        current = match entry {
            Value::Table(t) => t,
            Value::Array(items) => match items.last_mut() {
                Some(Value::Table(t)) => t,
                _ => {
                    return Err(TomlError {
                        line,
                        msg: format!("path segment {seg:?} addresses a non-table array"),
                    });
                }
            },
            _ => {
                return Err(TomlError {
                    line,
                    msg: format!("path segment {seg:?} addresses a non-table value"),
                });
            }
        };
    }
    Ok(current)
}

/// Parse a complete document into the root table.
pub fn parse(text: &str) -> Result<Table, TomlError> {
    let mut sc = Scanner::new(text);
    let mut root = Table::new();
    // Path of the currently open [table] / [[array-of-tables]] header.
    let mut current_path: Vec<String> = Vec::new();

    loop {
        sc.skip_trivia();
        match sc.peek() {
            None => return Ok(root),
            Some('[') => {
                sc.bump();
                let is_array = sc.peek() == Some('[');
                if is_array {
                    sc.bump();
                }
                let path = sc.parse_header_path()?;
                sc.skip_inline_ws();
                if sc.peek() != Some(']') {
                    return Err(sc.err("expected ']' closing table header"));
                }
                sc.bump();
                if is_array {
                    if sc.peek() != Some(']') {
                        return Err(sc.err("expected ']]' closing array-of-tables header"));
                    }
                    sc.bump();
                }
                let line = sc.line;
                sc.expect_eol()?;
                if is_array {
                    let (last, parents) = path
                        .split_last()
                        .ok_or_else(|| sc.err("empty array-of-tables header"))?;
                    let parent = navigate(&mut root, parents, line)?;
                    let entry = parent
                        .entry(last.clone())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    match entry {
                        Value::Array(items) => items.push(Value::Table(Table::new())),
                        _ => {
                            return Err(TomlError {
                                line,
                                msg: format!("key {last:?} is not an array of tables"),
                            });
                        }
                    }
                } else {
                    // Materialize the table (errors if the path is malformed).
                    navigate(&mut root, &path, line)?;
                }
                current_path = path;
            }
            Some(_) => {
                let key = sc.parse_key()?;
                sc.skip_inline_ws();
                if sc.peek() == Some('.') {
                    return Err(sc.err("dotted keys are outside the registry subset"));
                }
                if sc.peek() != Some('=') {
                    return Err(sc.err(format!("expected '=' after key {key:?}")));
                }
                sc.bump();
                sc.skip_inline_ws();
                let value = sc.parse_value()?;
                let line = sc.line;
                sc.expect_eol()?;
                let table = navigate(&mut root, &current_path, line)?;
                if table.contains_key(&key) {
                    return Err(TomlError {
                        line,
                        msg: format!("duplicate key {key:?}"),
                    });
                }
                table.insert(key, value);
            }
        }
    }
}

// ---- typed accessors used by the model layer ----

/// Typed read error for model construction (file-level, key-path addressed).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadError {
    pub path: String,
    pub msg: String,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.msg)
    }
}

impl std::error::Error for ReadError {}

pub fn get_str(table: &Table, key: &str, ctx: &str) -> Result<String, ReadError> {
    match table.get(key) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected string".into(),
        }),
        None => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "missing required key".into(),
        }),
    }
}

pub fn get_opt_str(table: &Table, key: &str, ctx: &str) -> Result<Option<String>, ReadError> {
    match table.get(key) {
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected string".into(),
        }),
        None => Ok(None),
    }
}

pub fn get_int(table: &Table, key: &str, ctx: &str) -> Result<i64, ReadError> {
    match table.get(key) {
        Some(Value::Int(v)) => Ok(*v),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected integer".into(),
        }),
        None => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "missing required key".into(),
        }),
    }
}

pub fn get_str_array(table: &Table, key: &str, ctx: &str) -> Result<Vec<String>, ReadError> {
    match table.get(key) {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::Str(s) => out.push(s.clone()),
                    _ => {
                        return Err(ReadError {
                            path: format!("{ctx}.{key}[{i}]"),
                            msg: "expected string".into(),
                        });
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected array of strings".into(),
        }),
        None => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "missing required key".into(),
        }),
    }
}

pub fn get_opt_str_array(
    table: &Table,
    key: &str,
    ctx: &str,
) -> Result<Option<Vec<String>>, ReadError> {
    if table.contains_key(key) {
        get_str_array(table, key, ctx).map(Some)
    } else {
        Ok(None)
    }
}

pub fn get_table<'a>(table: &'a Table, key: &str, ctx: &str) -> Result<&'a Table, ReadError> {
    match table.get(key) {
        Some(Value::Table(t)) => Ok(t),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected table".into(),
        }),
        None => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "missing required table".into(),
        }),
    }
}

pub fn get_table_array<'a>(
    table: &'a Table,
    key: &str,
    ctx: &str,
) -> Result<Vec<&'a Table>, ReadError> {
    match table.get(key) {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::Table(t) => out.push(t),
                    _ => {
                        return Err(ReadError {
                            path: format!("{ctx}.{key}[{i}]"),
                            msg: "expected table".into(),
                        });
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected array of tables".into(),
        }),
        None => Ok(Vec::new()),
    }
}
