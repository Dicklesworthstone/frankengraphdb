//! Statement-wide branch selection without interpreting the graph query.
//!
//! This layer owns only `AT BRANCH name` (or a text parameter). The remaining
//! statement goes through the existing native compiler, with byte offsets
//! preserved. Resolving a name to an authorized, immutable branch generation
//! belongs to the host; a selector is neither a branch catalog nor authority.

use crate::{GqlParameterValue, GqlParameters, MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_TEXT_TOKENS};
use fgdb_types::CanonicalScalar;
use std::borrow::Cow;

pub const MAX_GRAPH_BRANCH_NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphBranchTextErrorKind {
    DefinitionTooLarge,
    TooManyTokens,
    UnclosedQuote,
    UnclosedComment,
    UnbalancedDelimiter,
    DuplicateSelector,
    InvalidSelector,
    InvalidSelectorPosition,
    MultipleStatements,
    InvalidBranchName,
    MissingParameter,
    ParameterTypeMismatch,
    ArgumentMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphBranchTextError {
    pub offset: usize,
    pub kind: GraphBranchTextErrorKind,
}

impl core::fmt::Display for GraphBranchTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "branch selector error at byte {}: {:?}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphBranchTextError {}

fn error(offset: usize, kind: GraphBranchTextErrorKind) -> GraphBranchTextError {
    GraphBranchTextError { offset, kind }
}

#[derive(Clone)]
enum Selector {
    Literal(String),
    Parameter { name: String, used_in_query: bool },
}

/// An immutable outer selector and the still-uninterpreted native statement.
///
/// A selector may precede MATCH, follow MATCH or a completed pattern, or follow
/// the complete read statement. It selects the whole statement, including all
/// UNION operands. Nested selectors and per-operand branch switching are not
/// admitted by this facade. Quoted names use doubled quotes, not interpolation.
#[derive(Clone)]
pub struct PreparedGraphBranchText {
    statement: String,
    selector: Option<Selector>,
    selector_offset: usize,
}

impl core::fmt::Debug for PreparedGraphBranchText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphBranchText")
            .field("has_selector", &self.selector.is_some())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// Values are retained as values, never inserted into the native statement.
/// Only a parameter used exclusively by the selector is removed from the
/// native argument map. All other arguments, including unknown ones, survive
/// so native schema admission cannot be bypassed by the routing layer.
pub struct BoundGraphBranchText<'a> {
    branch: Option<String>,
    statement: &'a str,
    parameters: Cow<'a, GqlParameters>,
}

impl core::fmt::Debug for BoundGraphBranchText<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundGraphBranchText")
            .field("has_selector", &self.branch.is_some())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl BoundGraphBranchText<'_> {
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        self.statement
    }

    #[must_use]
    pub fn parameters(&self) -> &GqlParameters {
        &self.parameters
    }
}

impl PreparedGraphBranchText {
    pub fn prepare(text: &str) -> Result<Self, GraphBranchTextError> {
        if text.len() > MAX_GRAPH_TEXT_BYTES {
            return Err(error(MAX_GRAPH_TEXT_BYTES, GraphBranchTextErrorKind::DefinitionTooLarge));
        }
        let tokens = tokens(text)?;
        let mut selected = None;
        for (index, pair) in tokens.windows(2).enumerate() {
            if pair[0].word("AT") && pair[1].word("BRANCH") {
                if pair[0].depth != 0 || pair[1].depth != 0 {
                    return Err(error(pair[0].start, GraphBranchTextErrorKind::InvalidSelectorPosition));
                }
                if selected.replace(index).is_some() {
                    return Err(error(pair[0].start, GraphBranchTextErrorKind::DuplicateSelector));
                }
            }
        }
        let Some(index) = selected else {
            return Ok(Self { statement: text.to_owned(), selector: None, selector_offset: 0 });
        };
        let first = tokens[index];
        let value = tokens.get(index + 2).copied()
            .ok_or_else(|| error(first.start, GraphBranchTextErrorKind::InvalidSelector))?;
        if value.depth != 0 {
            return Err(error(value.start, GraphBranchTextErrorKind::InvalidSelector));
        }
        // Do not turn a selector-shaped fragment inside a RETURN expression,
        // alias, or UNION operand into an otherwise valid graph statement.
        let prefix = index == 0;
        let suffix = index + 3 == tokens.len()
            || (index + 4 == tokens.len() && tokens[index + 3].punct(b';'));
        let after_match = index > 0 && tokens[index - 1].word("MATCH");
        let after_pattern = index > 0 && tokens[index - 1].punct(b')')
            && tokens.get(index + 3).is_some_and(|next| {
                ["WHERE", "RETURN", "WITH", "MATCH", "OPTIONAL", "FOR"]
                    .iter().any(|word| next.word(word))
            });
        if !(prefix || suffix || after_match || after_pattern) {
            return Err(error(first.start, GraphBranchTextErrorKind::InvalidSelectorPosition));
        }
        // Branch selection is statement-wide, never local to a set operand.
        if tokens[..index].iter().any(|token| token.depth == 0 && token.word("UNION"))
            && !suffix
        {
            return Err(error(first.start, GraphBranchTextErrorKind::InvalidSelectorPosition));
        }
        let selector = match value.kind {
            Kind::Word(name) => Selector::Literal(branch_name(name, value.start)?),
            Kind::Quoted(raw, quote) => {
                // Even a name consisting entirely of doubled delimiters has
                // at most twice the admitted decoded byte length.
                if raw.len() > 2 * MAX_GRAPH_BRANCH_NAME_BYTES {
                    return Err(error(value.start, GraphBranchTextErrorKind::InvalidBranchName));
                }
                let (doubled, single) = match quote {
                    b'\'' => ("''", "'"),
                    b'"' => ("\"\"", "\""),
                    b'`' => ("``", "`"),
                    _ => unreachable!("scanner admits only three quoted delimiters"),
                };
                let decoded = raw.replace(doubled, single);
                Selector::Literal(branch_name(&decoded, value.start)?)
            }
            Kind::Parameter(name) if !name.is_empty()
                && name.len() <= crate::parameters::MAX_GQL_PARAMETER_NAME_BYTES => Selector::Parameter {
                name: name.to_owned(),
                used_in_query: tokens.iter().enumerate().any(|(at, token)| {
                    at != index + 2 && matches!(token.kind, Kind::Parameter(other) if other == name)
                }),
            },
            _ => return Err(error(value.start, GraphBranchTextErrorKind::InvalidSelector)),
        };
        let mut statement = text.to_owned();
        // The replacement has the same byte length, including quoted UTF-8
        // names. Native offsets remain offsets into the caller's source text.
        statement.replace_range(first.start..value.end, &" ".repeat(value.end - first.start));
        Ok(Self { statement, selector: Some(selector), selector_offset: value.start })
    }

    #[must_use]
    pub fn has_selector(&self) -> bool {
        self.selector.is_some()
    }

    /// The selector-free statement is an explicit plaintext export. It has not
    /// been admitted as graph syntax; only a native query compiler can do that.
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    pub fn bind_parameters<'a>(
        &'a self,
        arguments: &'a GqlParameters,
    ) -> Result<BoundGraphBranchText<'a>, GraphBranchTextError> {
        let mut parameters = Cow::Borrowed(arguments);
        let branch = match &self.selector {
            None => None,
            Some(Selector::Literal(name)) => Some(name.clone()),
            Some(Selector::Parameter { name, used_in_query }) => {
                let value = arguments.get(name)
                    .ok_or_else(|| error(self.selector_offset, GraphBranchTextErrorKind::MissingParameter))?;
                let GqlParameterValue::Scalar(value) = value else {
                    return Err(error(self.selector_offset, GraphBranchTextErrorKind::ParameterTypeMismatch));
                };
                let CanonicalScalar::Text(text) = value.value() else {
                    return Err(error(self.selector_offset, GraphBranchTextErrorKind::ParameterTypeMismatch));
                };
                let branch = branch_name(text.as_str(), self.selector_offset)?;
                if !*used_in_query {
                    let mut native = GqlParameters::new();
                    for (other, _) in arguments.parameter_types() {
                        if other != name.as_str() {
                            // This is a subset of an already admitted map.
                            let value = arguments.get(other)
                                .expect("parameter_types enumerates retained arguments");
                            native.insert(other, value)
                                .map_err(|_| error(self.selector_offset, GraphBranchTextErrorKind::ArgumentMap))?;
                        }
                    }
                    parameters = Cow::Owned(native);
                }
                Some(branch)
            }
        };
        Ok(BoundGraphBranchText { branch, statement: &self.statement, parameters })
    }
}

fn branch_name(name: &str, at: usize) -> Result<String, GraphBranchTextError> {
    if name.is_empty() || name.len() > MAX_GRAPH_BRANCH_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(error(at, GraphBranchTextErrorKind::InvalidBranchName));
    }
    Ok(name.to_owned())
}

#[derive(Clone, Copy)]
enum Kind<'a> {
    Word(&'a str),
    Parameter(&'a str),
    Quoted(&'a str, u8),
    Punct(u8),
    Other,
}

#[derive(Clone, Copy)]
struct Token<'a> {
    kind: Kind<'a>,
    start: usize,
    end: usize,
    depth: usize,
}
impl Token<'_> {
    fn word(self, word: &str) -> bool {
        matches!(self.kind, Kind::Word(actual) if actual.eq_ignore_ascii_case(word))
    }
    fn punct(self, byte: u8) -> bool {
        matches!(self.kind, Kind::Punct(actual) if actual == byte)
    }
}

// This scanner recognizes selector boundaries only. It neither accepts graph
// expressions nor rewrites them; unsupported native syntax remains untouched.
fn tokens(text: &str) -> Result<Vec<Token<'_>>, GraphBranchTextError> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut stack = Vec::new();
    let mut result = Vec::new();
    while at < bytes.len() {
        let ch = text[at..].chars().next().expect("at is a UTF-8 boundary");
        if ch.is_whitespace() { at += ch.len_utf8(); continue; }
        if bytes[at..].starts_with(b"//") {
            at += text[at..].find('\n').unwrap_or(bytes.len() - at);
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            let start = at;
            let end = text[at + 2..].find("*/")
                .ok_or_else(|| error(start, GraphBranchTextErrorKind::UnclosedComment))?;
            at += end + 4;
            continue;
        }
        if result.len() == MAX_GRAPH_TEXT_TOKENS {
            return Err(error(at, GraphBranchTextErrorKind::TooManyTokens));
        }
        let start = at;
        let depth = stack.len();
        let byte = bytes[at];
        let kind = if [b'\'', b'"', b'`'].contains(&byte) {
            at += 1;
            let body = at;
            loop {
                let Some(next) = bytes.get(at) else {
                    return Err(error(start, GraphBranchTextErrorKind::UnclosedQuote));
                };
                if *next == byte {
                    if bytes.get(at + 1) == Some(&byte) { at += 2; continue; }
                    break;
                }
                at += 1;
            }
            let raw = &text[body..at];
            at += 1;
            Kind::Quoted(raw, byte)
        } else if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' {
            let parameter = byte == b'$';
            if parameter { at += 1; }
            let body = at;
            if bytes.get(at).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') {
                at += 1;
                while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') { at += 1; }
            }
            if parameter { Kind::Parameter(&text[body..at]) } else { Kind::Word(&text[body..at]) }
        } else {
            at += ch.len_utf8();
            match byte {
                b'(' => stack.push((b')', start)),
                b'[' => stack.push((b']', start)),
                b'{' => stack.push((b'}', start)),
                b')' | b']' | b'}' => {
                    if stack.pop().map(|(expected, _)| expected) != Some(byte) {
                        return Err(error(start, GraphBranchTextErrorKind::UnbalancedDelimiter));
                    }
                }
                _ => {}
            }
            if byte.is_ascii() { Kind::Punct(byte) } else { Kind::Other }
        };
        result.push(Token { kind, start, end: at, depth });
    }
    if let Some((_, start)) = stack.last() {
        return Err(error(*start, GraphBranchTextErrorKind::UnbalancedDelimiter));
    }
    for token in result.iter().take(result.len().saturating_sub(1)) {
        if token.depth == 0 && token.punct(b';') {
            return Err(error(token.start, GraphBranchTextErrorKind::MultipleStatements));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_and_utf8_names_preserve_native_byte_offsets() {
        for source in [
            "AT BRANCH 'résumé''s' MATCH (n) RETURN n.name",
            "MATCH AT BRANCH 'résumé''s' (n) RETURN n.name",
            "MATCH (n) AT BRANCH 'résumé''s' RETURN n.name",
            "MATCH (n) RETURN n.name AT BRANCH 'résumé''s'",
        ] {
            let prepared = PreparedGraphBranchText::prepare(source).unwrap();
            let args = GqlParameters::new();
            let bound = prepared.bind_parameters(&args).unwrap();
            assert_eq!(bound.branch(), Some("résumé's"));
            assert_eq!(bound.statement().len(), source.len());
            assert_eq!(bound.statement().find("n.name"), source.find("n.name"));
            assert!(!bound.statement().contains("BRANCH"));
        }
    }

    #[test]
    fn parameter_values_are_opaque_and_only_selector_only_arguments_are_consumed() {
        let payload = "prod' RETURN secret; MATCH (x)";
        let args = GqlParameters::new().with_text("branch", payload).unwrap()
            .with_int64("age", 7).unwrap().with_int64("unknown", 9).unwrap();
        let before = args.clone();
        let prepared = PreparedGraphBranchText::prepare(
            "AT BRANCH $branch MATCH (n) WHERE n.age = $age RETURN n.name"
        ).unwrap();
        let bound = prepared.bind_parameters(&args).unwrap();
        assert_eq!(bound.branch(), Some(payload));
        assert!(!bound.statement().contains(payload));
        assert_eq!(bound.parameters().len(), 2);
        assert!(bound.parameters().get("branch").is_none());
        assert!(bound.parameters().get("unknown").is_some());
        assert_eq!(args, before);
        let reused = PreparedGraphBranchText::prepare(
            "AT BRANCH $branch MATCH (n) WHERE n.name = $branch RETURN n"
        ).unwrap();
        assert_eq!(reused.bind_parameters(&args).unwrap().parameters(), &args);
    }

    #[test]
    fn quoted_and_commented_selector_lookalikes_do_not_route() {
        for source in [
            "MATCH (n {name:'AT BRANCH hidden'}) RETURN n",
            "MATCH (n) RETURN n AS `AT BRANCH hidden`",
            "MATCH (n) RETURN 'AT BRANCH hidden' AS name",
            "/* AT BRANCH hidden */ MATCH (n) RETURN n",
            "MATCH (n) RETURN n // AT BRANCH hidden",
        ] {
            let prepared = PreparedGraphBranchText::prepare(source).unwrap();
            assert!(!prepared.has_selector());
            assert_eq!(prepared.statement(), source);
        }
    }

    #[test]
    fn malformed_duplicate_nested_and_expression_selectors_refuse() {
        for source in [
            "AT BRANCH",
            "AT BRANCH $ MATCH (n) RETURN n",
            "AT BRANCH 42 MATCH (n) RETURN n",
            "AT BRANCH '' MATCH (n) RETURN n",
            "AT BRANCH a MATCH (n) RETURN n AT BRANCH a",
            "MATCH (n) WHERE EXISTS { AT BRANCH a MATCH (m) RETURN m } RETURN n",
            "MATCH (n) RETURN n AS AT BRANCH a alias",
            "MATCH (n) RETURN n UNION MATCH AT BRANCH a (m) RETURN m",
            "AT BRANCH a MATCH (n] RETURN n",
            "AT BRANCH 'unclosed",
            "/* unclosed",
            "AT BRANCH a MATCH (n) RETURN n; MATCH (m) RETURN m",
        ] {
            assert!(PreparedGraphBranchText::prepare(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn selectors_require_present_nonnull_bounded_text() {
        let prepared = PreparedGraphBranchText::prepare("AT BRANCH $b MATCH (n) RETURN n").unwrap();
        for args in [
            GqlParameters::new(),
            GqlParameters::new().with_int64("b", 1).unwrap(),
            GqlParameters::new().with_null("b").unwrap(),
            GqlParameters::new().with_text("b", "").unwrap(),
            GqlParameters::new().with_text("b", "a\0b").unwrap(),
            GqlParameters::new().with_text("b", &"x".repeat(MAX_GRAPH_BRANCH_NAME_BYTES + 1)).unwrap(),
        ] {
            assert!(prepared.bind_parameters(&args).is_err());
        }
        let args = GqlParameters::new().with_text("b", &"x".repeat(MAX_GRAPH_BRANCH_NAME_BYTES)).unwrap();
        assert!(prepared.bind_parameters(&args).is_ok());
    }

    #[test]
    fn definitions_and_scan_work_are_bounded_before_binding() {
        assert_eq!(PreparedGraphBranchText::prepare(&" ".repeat(MAX_GRAPH_TEXT_BYTES + 1)).unwrap_err().kind,
            GraphBranchTextErrorKind::DefinitionTooLarge);
        assert_eq!(PreparedGraphBranchText::prepare(&"x ".repeat(MAX_GRAPH_TEXT_TOKENS + 1)).unwrap_err().kind,
            GraphBranchTextErrorKind::TooManyTokens);
    }

    #[test]
    fn debug_and_errors_never_reveal_names_or_arguments() {
        let prepared = PreparedGraphBranchText::prepare("AT BRANCH private_name MATCH (n) RETURN n").unwrap();
        let args = GqlParameters::new();
        let bound = prepared.bind_parameters(&args).unwrap();
        assert!(!format!("{prepared:?} {bound:?}").contains("private_name"));
        let failure = PreparedGraphBranchText::prepare("AT BRANCH private_name AT BRANCH private_name").unwrap_err();
        assert!(!format!("{failure} {failure:?}").contains("private_name"));
    }
}
