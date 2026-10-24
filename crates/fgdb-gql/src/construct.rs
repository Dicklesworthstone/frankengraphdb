//! Statement-level constructs no native compiler implements yet, named before
//! a compiler refuses them at a byte that points elsewhere (`CREATE GRAPH g`
//! otherwise reads as a CREATE pattern missing its `(`).
//!
//! A keyword scan under the shared quoting and comment rules, not a grammar:
//! it reports only keyword sequences that no supported statement contains, so
//! it never refuses a statement a compiler would accept. A word in a property
//! or label position (after `.` or `:`) is a name, never a keyword.

use crate::GqlRefusalClass;

/// The first unsupported construct in a statement and the byte where its
/// first keyword starts. Carries no statement text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlUnsupportedConstruct {
    pub offset: usize,
    pub kind: GqlUnsupportedConstructKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlUnsupportedConstructKind {
    /// `CREATE [OR REPLACE] [PROPERTY] GRAPH`: graph catalog DDL.
    CreateGraph,
    /// `DROP [PROPERTY] GRAPH`.
    DropGraph,
    /// `CREATE BRANCH`: branch DDL.
    CreateBranch,
    /// `MERGE BRANCH`.
    MergeBranch,
    /// `DROP BRANCH`.
    DropBranch,
    /// `AT BRANCH`, reported only for a host without a branch catalog.
    BranchSelector,
}

impl core::fmt::Display for GqlUnsupportedConstructKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::CreateGraph => "CREATE GRAPH is not supported yet (graph DDL)",
            Self::DropGraph => "DROP GRAPH is not supported yet (graph DDL)",
            Self::CreateBranch => "CREATE BRANCH is not supported yet (branch DDL)",
            Self::MergeBranch => "MERGE BRANCH is not supported yet (branch DDL)",
            Self::DropBranch => "DROP BRANCH is not supported yet (branch DDL)",
            Self::BranchSelector => "AT BRANCH is not supported here: there is no branch catalog",
        })
    }
}
impl core::fmt::Display for GqlUnsupportedConstruct {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GqlUnsupportedConstruct {}

impl GqlUnsupportedConstruct {
    /// The first unsupported construct in `text`, scanning every statement of
    /// a script. `branches` says whether the host resolves `AT BRANCH`; a host
    /// without a branch catalog has it reported too.
    #[must_use]
    pub fn find(text: &str, branches: bool) -> Option<Self> {
        let tokens = tokens(text);
        tokens.iter().enumerate().find_map(|(index, token)| {
            let Token::Word(word, offset) = *token else {
                return None;
            };
            if index > 0 && matches!(tokens[index - 1], Token::Punct(b'.' | b':')) {
                return None;
            }
            let next = |skip: usize| tokens.get(index + 1 + skip).copied();
            let is = |skip: usize, expected: &str| {
                matches!(next(skip), Some(Token::Word(word, _)) if word.eq_ignore_ascii_case(expected))
            };
            let kind = if word.eq_ignore_ascii_case("CREATE") {
                // CREATE [OR REPLACE] [PROPERTY] GRAPH | CREATE BRANCH
                let mut skip = if is(0, "OR") && is(1, "REPLACE") { 2 } else { 0 };
                if is(skip, "PROPERTY") {
                    skip += 1;
                }
                if is(skip, "GRAPH") {
                    GqlUnsupportedConstructKind::CreateGraph
                } else if is(0, "BRANCH") {
                    GqlUnsupportedConstructKind::CreateBranch
                } else {
                    return None;
                }
            } else if word.eq_ignore_ascii_case("DROP") {
                let skip = usize::from(is(0, "PROPERTY"));
                if is(skip, "GRAPH") {
                    GqlUnsupportedConstructKind::DropGraph
                } else if is(0, "BRANCH") {
                    GqlUnsupportedConstructKind::DropBranch
                } else {
                    return None;
                }
            } else if word.eq_ignore_ascii_case("MERGE") && is(0, "BRANCH") {
                GqlUnsupportedConstructKind::MergeBranch
            } else if word.eq_ignore_ascii_case("AT") && is(0, "BRANCH") && !branches {
                GqlUnsupportedConstructKind::BranchSelector
            } else {
                return None;
            };
            Some(Self { offset, kind })
        })
    }

    #[must_use]
    pub fn class(&self) -> GqlRefusalClass {
        GqlRefusalClass::Unsupported
    }
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Word(&'a str, usize),
    Punct(u8),
    /// A parameter, number or non-ASCII character: never part of a keyword.
    Other,
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if let Some(end) = crate::graph_text::literal::opaque_end(text, at) {
            // A quoted span is a value or a name; a comment is trivia.
            if b"'\"`".contains(&byte) {
                result.push(Token::Other);
            }
            at = end;
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = at;
            while bytes
                .get(at)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                at += 1;
            }
            result.push(Token::Word(&text[start..at], start));
        } else if byte == b'$' || byte.is_ascii_digit() {
            at += 1;
            while bytes
                .get(at)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                at += 1;
            }
            result.push(Token::Other);
        } else {
            let ch = text[at..].chars().next().expect("at is a UTF-8 boundary");
            at += ch.len_utf8();
            if ch.is_whitespace() {
                continue;
            }
            result.push(if byte.is_ascii() {
                Token::Punct(byte)
            } else {
                Token::Other
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use GqlUnsupportedConstructKind as K;

    fn found(text: &str) -> Option<(usize, GqlUnsupportedConstructKind)> {
        GqlUnsupportedConstruct::find(text, false).map(|found| (found.offset, found.kind))
    }

    #[test]
    fn names_each_construct_at_its_first_keyword() {
        for (text, expected) in [
            ("CREATE GRAPH social", (0, K::CreateGraph)),
            ("create or replace property graph g", (0, K::CreateGraph)),
            ("  DROP GRAPH social", (2, K::DropGraph)),
            ("DROP PROPERTY GRAPH social", (0, K::DropGraph)),
            (
                "CREATE BRANCH what_if FROM social@trunk",
                (0, K::CreateBranch),
            ),
            ("MERGE BRANCH what_if INTO trunk", (0, K::MergeBranch)),
            ("DROP BRANCH what_if", (0, K::DropBranch)),
            (
                "MATCH (p) AT BRANCH what_if RETURN p",
                (10, K::BranchSelector),
            ),
            ("INSERT (:A); CREATE GRAPH g", (13, K::CreateGraph)),
        ] {
            assert_eq!(found(text), Some(expected), "{text}");
        }
    }

    /// Supported statements, and the same keywords as names, text, comments
    /// or parameters, are never reported.
    #[test]
    fn supported_statements_and_quoted_keywords_pass() {
        for text in [
            "CREATE (:Person {name: 'CREATE GRAPH g'})",
            "MERGE (b:Branch {name: \"x\"})",
            "MATCH (n) WHERE n.call = 1 RETURN n /* CALL { } */",
            "MATCH (n:CREATE) RETURN n.graph // DROP GRAPH g",
            "CALL fnx.pagerank() YIELD node, score RETURN node, score",
            "CALL { MATCH (p) RETURN p } RETURN count(p)",
            "MATCH (p) CALL (p) { RETURN 1 AS x } RETURN x",
            "MATCH (n) RETURN n.create, `CREATE GRAPH`",
            "MATCH (n) WHERE n.x = $call RETURN n",
            "MATCH (n) RETURN [x IN [1, 2] | x] AS xs",
        ] {
            assert_eq!(found(text), None, "{text}");
        }
        assert_eq!(
            GqlUnsupportedConstruct::find("MATCH (p) AT BRANCH b RETURN p", true),
            None
        );
    }

    #[test]
    fn messages_carry_no_statement_text() {
        let found = GqlUnsupportedConstruct::find("CREATE GRAPH secret_name", false).unwrap();
        assert_eq!(
            found.to_string(),
            "byte 0: CREATE GRAPH is not supported yet (graph DDL)"
        );
        assert_eq!(found.class(), GqlRefusalClass::Unsupported);
    }
}
