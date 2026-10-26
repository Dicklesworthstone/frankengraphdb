//! Map arguments as property maps (fgdb-0s2pq).
//!
//! Drivers send a record's properties as one map parameter: `SET n += $props`,
//! `SET n = $props`, `CREATE (n:Person $props)`, `MERGE (n:Person $props)`.
//! Property keys resolve through the catalog when a statement is prepared,
//! and these keys exist only in the argument, so only a caller that holds the
//! arguments can bind such a statement. There each such `$name` is spliced as
//! the literal map of its argument, one fresh scalar parameter per key:
//! `SET n += {`a`: $aa, `b`: $ab}`. A literal map is the existing syntax for
//! simultaneous property assignments, so every compiler keeps its own meaning
//! for it, including `SET n = {}` unsetting every property.
//!
//! A site is lexical: a SET item that is exactly `name += $p` or `name = $p`,
//! or a top-level node or relationship pattern of a MATCH, CREATE, MERGE or
//! INSERT clause whose last token before `)` or `]` is `$p` and which holds
//! only a variable, labels or a length before it. Only a map argument lowers.
//! A statement with no such site, or one the lexer refuses, binds as written,
//! so its existing diagnostic stands; so does one whose argument caps refuse
//! the expansion. Diagnostics of the lowered statement map back through
//! `original_offset`: every byte of a spliced map is the `$` of its parameter.
//!
//! The lowered statement retains no argument value. A caller that prepares
//! once and binds many times derives each binding's arguments through
//! `arguments`, which refuses a map whose keys differ from the prepared ones:
//! a missing key would assign NULL where `+=` leaves the property unchanged.

use super::{Lexer, Token, TokenKind};
use crate::algebra::{GraphValue, MAX_PATTERN_NAME_BYTES};
use crate::unwind_write::{
    UnwindSourceOffset, fresh_parameter, original_offset_in, restore_script_error,
};
use crate::{
    GqlListParameter, GqlMapParameter, GqlParameterError, GqlParameterValue, GqlParameters,
    GqlScalarParameter,
};
use std::collections::{BTreeMap, BTreeSet};

/// A statement whose map arguments at property-map sites are spliced as
/// literal maps of fresh parameters, one per key.
pub struct GqlMapArgumentText {
    text: String,
    edits: Box<[UnwindSourceOffset]>,
    /// Each spliced map, in text order.
    maps: Box<[SplicedMap]>,
    /// The arguments no remaining token reads.
    spliced: Box<[Box<str>]>,
}

/// One spliced map: its argument and, for every key in order, the parameter
/// the key binds.
struct SplicedMap {
    parameter: Box<str>,
    keys: Box<[(Box<str>, Box<str>)]>,
}

/// Why a binding's arguments do not fit a lowered statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlMapArgumentError {
    /// The argument a spliced map stands for is absent or not a map.
    ExpectedMap {
        parameter: Box<str>,
    },
    /// The argument's keys differ from the keys the statement was lowered for.
    KeysDiffer {
        parameter: Box<str>,
    },
    /// A value no parameter can carry: a graph element, for one.
    UnsupportedValue {
        parameter: Box<str>,
    },
    Parameter(GqlParameterError),
}
impl core::fmt::Display for GqlMapArgumentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExpectedMap { parameter } => {
                write!(
                    f,
                    "${parameter} must be a map, as when the statement was prepared"
                )
            }
            Self::KeysDiffer { parameter } => write!(
                f,
                "${parameter} must have the keys the statement was prepared with"
            ),
            Self::UnsupportedValue { parameter } => {
                write!(f, "${parameter} holds a value no property can store")
            }
            Self::Parameter(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GqlMapArgumentError {}

impl GqlMapArgumentText {
    /// The lowered statement and the arguments that bind it, or `None` when
    /// no property-map site holds a map argument: bind `text` with
    /// `arguments` as they are.
    #[must_use]
    pub fn lower(text: &str, arguments: &GqlParameters) -> Option<(Self, GqlParameters)> {
        let lowered = Self::splice(text, arguments)?;
        let derived = lowered.arguments(arguments).ok()?;
        Some((lowered, derived))
    }

    fn splice(text: &str, arguments: &GqlParameters) -> Option<Self> {
        let tokens = tokens(text)?;
        let sites: Vec<(usize, &str, GqlMapParameter)> = property_map_sites(&tokens)
            .into_iter()
            .filter_map(|site| {
                let TokenKind::Parameter(name) = tokens[site].kind else {
                    return None;
                };
                let Some(GqlParameterValue::Map(map)) = arguments.get(name) else {
                    return None;
                };
                let delimited = map.entries().all(|(key, _)| delimitable(key));
                delimited.then_some((site, name, map))
            })
            .collect();
        if sites.is_empty() {
            return None;
        }

        // An argument leaves the binding only when every token naming it
        // was spliced; `SET n += $m RETURN $m.k` still reads `$m`.
        let mut occurrences: BTreeMap<&str, usize> = BTreeMap::new();
        for token in &tokens {
            if let TokenKind::Parameter(name) = token.kind {
                *occurrences.entry(name).or_default() += 1;
            }
        }
        for (_, name, _) in &sites {
            if let Some(count) = occurrences.get_mut(name) {
                *count -= 1;
            }
        }
        let mut reserved: BTreeSet<String> = (arguments.parameter_types())
            .map(|(name, _)| name.to_owned())
            .chain(occurrences.keys().map(|name| (*name).to_owned()))
            .collect();
        let spliced = (occurrences.iter())
            .filter(|&(_, &count)| count == 0)
            .map(|(name, _)| (*name).into())
            .collect();

        let mut lowered = String::with_capacity(text.len());
        let mut edits = Vec::with_capacity(sites.len());
        let mut maps = Vec::with_capacity(sites.len());
        let mut cursor = 0;
        let mut copied = 0;
        for (site, name, map) in &sites {
            let start = tokens[*site].at;
            let end = start + 1 + name.len();
            lowered.push_str(&text[copied..start]);
            let generated = lowered.len();
            lowered.push('{');
            let mut keys = Vec::new();
            for (index, (key, _)) in map.entries().enumerate() {
                let parameter = fresh_parameter(&mut reserved, &mut cursor)?;
                if index != 0 {
                    lowered.push_str(", ");
                }
                lowered.push('`');
                lowered.push_str(key);
                lowered.push_str("`: $");
                lowered.push_str(&parameter);
                keys.push((key.into(), parameter.into()));
            }
            lowered.push('}');
            edits.push(UnwindSourceOffset {
                generated: generated..lowered.len(),
                original: start..end,
            });
            maps.push(SplicedMap {
                parameter: (*name).into(),
                keys: keys.into_boxed_slice(),
            });
            copied = end;
        }
        lowered.push_str(&text[copied..]);
        Some(Self {
            text: lowered,
            edits: edits.into_boxed_slice(),
            maps: maps.into_boxed_slice(),
            spliced,
        })
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The arguments that bind the lowered statement for one binding of the
    /// caller's: each argument a remaining token reads, plus one per spliced
    /// key, read from that binding's map. Its keys must be the lowered ones.
    pub fn arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<GqlParameters, GqlMapArgumentError> {
        let mut derived = GqlParameters::new();
        for (name, _) in arguments.parameter_types() {
            if !self.spliced.iter().any(|spliced| **spliced == *name)
                && let Some(value) = arguments.get(name)
            {
                derived
                    .insert(name, value)
                    .map_err(GqlMapArgumentError::Parameter)?;
            }
        }
        for SplicedMap { parameter, keys } in &self.maps {
            let Some(GqlParameterValue::Map(map)) = arguments.get(parameter) else {
                return Err(GqlMapArgumentError::ExpectedMap {
                    parameter: parameter.clone(),
                });
            };
            if map.entries().count() != keys.len()
                || map
                    .entries()
                    .zip(keys)
                    .any(|((key, _), (lowered, _))| key != &**lowered)
            {
                return Err(GqlMapArgumentError::KeysDiffer {
                    parameter: parameter.clone(),
                });
            }
            for ((_, value), (_, name)) in map.entries().zip(keys) {
                let value =
                    argument(value).ok_or_else(|| GqlMapArgumentError::UnsupportedValue {
                        parameter: parameter.clone(),
                    })?;
                derived
                    .insert(&**name, value)
                    .map_err(GqlMapArgumentError::Parameter)?;
            }
        }
        Ok(derived)
    }

    /// The byte of the caller's statement that `generated`, a byte of the
    /// lowered one, stands for.
    #[must_use]
    pub fn original_offset(&self, generated: usize) -> usize {
        original_offset_in(&self.edits, generated)
    }

    /// Report `error`, raised by the lowered statement, at the caller's bytes.
    pub fn restore<E: GqlTextOffsets + ?Sized>(&self, error: &mut E) {
        error.restore_offsets(&|generated| self.original_offset(generated));
    }
}

/// A statement-text error whose bytes a lowered statement remaps
/// (GqlMapArgumentText::restore).
pub trait GqlTextOffsets {
    fn restore_offsets(&mut self, original: &dyn Fn(usize) -> usize);
}

macro_rules! text_offsets {
    ($($ty:ty),+ $(,)?) => {
        $(impl GqlTextOffsets for $ty {
            fn restore_offsets(&mut self, original: &dyn Fn(usize) -> usize) {
                self.offset = original(self.offset);
            }
        })+
    };
}
text_offsets!(
    super::GraphPatternTextError,
    crate::GraphSetTextError,
    crate::GraphInsertTextError,
    crate::GraphMutationTextError,
    crate::GraphVertexMergeTextError,
    crate::GraphEdgeMergeTextError,
    crate::GraphVertexUpsertTextError,
    crate::GraphEdgeUpsertTextError,
);

impl GqlTextOffsets for crate::GraphWriteScriptError {
    fn restore_offsets(&mut self, original: &dyn Fn(usize) -> usize) {
        restore_script_error(self, original);
    }
}

impl GqlTextOffsets for crate::GraphWriteChainTextError {
    fn restore_offsets(&mut self, original: &dyn Fn(usize) -> usize) {
        match self {
            Self::Insert(error) => error.restore_offsets(original),
            Self::EdgeMerge(error) => error.restore_offsets(original),
            Self::VertexMerge(error) => error.restore_offsets(original),
            Self::Mutation(error) => error.restore_offsets(original),
            Self::Read(error) => error.restore_offsets(original),
        }
    }
}

fn tokens(text: &str) -> Option<Vec<Token<'_>>> {
    let mut lexer = Lexer {
        text,
        at: 0,
        tokens: 0,
    };
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next().ok()?;
        let end = matches!(token.kind, TokenKind::End);
        tokens.push(token);
        if end {
            return Some(tokens);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    Other,
    Pattern,
    Set,
}

const PATTERN_CLAUSES: &[&str] = &["MATCH", "CREATE", "MERGE", "INSERT"];
const OTHER_CLAUSES: &[&str] = &[
    "WHERE", "RETURN", "WITH", "UNWIND", "ON", "DELETE", "DETACH", "REMOVE", "ORDER", "SKIP",
    "LIMIT", "OFFSET", "CALL", "UNION", "YIELD", "FOREACH", "OPTIONAL", "FINISH", "FILTER", "LET",
    "NEXT", "USE", "FOR",
];

fn is_clause(word: &str) -> bool {
    let named = |words: &[&str]| words.iter().any(|clause| word.eq_ignore_ascii_case(clause));
    word.eq_ignore_ascii_case("SET") || named(PATTERN_CLAUSES) || named(OTHER_CLAUSES)
}

/// The parameter tokens standing where a property map does, in text order.
/// Only top-level clauses count: a subquery's braces hide its own.
fn property_map_sites(tokens: &[Token<'_>]) -> Vec<usize> {
    let mut sites = Vec::new();
    let mut clause = Clause::Other;
    let mut depth = 0_usize;
    let mut item = None;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => {
                if depth == 0 && clause == Clause::Pattern && open != b'{' {
                    sites.extend(pattern_site(tokens, index, open));
                }
                depth += 1;
            }
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            TokenKind::Punct(b',') if depth == 0 && clause == Clause::Set => item = Some(index + 1),
            TokenKind::Word(word) if depth == 0 && is_clause(word) => {
                if word.eq_ignore_ascii_case("SET") {
                    clause = Clause::Set;
                    item = Some(index + 1);
                } else if PATTERN_CLAUSES.iter().any(|k| word.eq_ignore_ascii_case(k)) {
                    clause = Clause::Pattern;
                } else {
                    clause = Clause::Other;
                }
            }
            TokenKind::Word(_) if clause == Clause::Set && item == Some(index) => {
                sites.extend(set_site(tokens, index));
            }
            _ => {}
        }
    }
    sites
}

/// `name += $p` or `name = $p`, ending the SET item that starts at `start`.
fn set_site(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    let mut next = start + 1;
    if matches!(tokens.get(next)?.kind, TokenKind::Punct(b'+')) {
        next += 1;
    }
    if !matches!(tokens.get(next)?.kind, TokenKind::Punct(b'=')) {
        return None;
    }
    let parameter = next + 1;
    let TokenKind::Parameter(_) = tokens.get(parameter)?.kind else {
        return None;
    };
    match tokens.get(parameter + 1)?.kind {
        TokenKind::End | TokenKind::Punct(b',' | b';') => Some(parameter),
        TokenKind::Word(word) if is_clause(word) => Some(parameter),
        _ => None,
    }
}

/// `(n:L $p)` or `[r:T*1..2 $p]`: the parameter closes a pattern that holds
/// only a variable, labels or a length before it.
fn pattern_site(tokens: &[Token<'_>], open: usize, delimiter: u8) -> Option<usize> {
    let close = if delimiter == b'(' { b')' } else { b']' };
    let mut at = open + 1;
    while matches!(
        tokens.get(at)?.kind,
        TokenKind::Word(_)
            | TokenKind::DelimitedKeyword(_)
            | TokenKind::Digits(_)
            | TokenKind::Punct(b':' | b'|' | b'&' | b'!' | b'*' | b'.' | b'%')
    ) {
        at += 1;
    }
    let TokenKind::Parameter(_) = tokens.get(at)?.kind else {
        return None;
    };
    matches!(tokens.get(at + 1)?.kind, TokenKind::Punct(c) if c == close).then_some(at)
}

/// A key the lowered text can delimit: `` `key` `` lexes as that name.
fn delimitable(key: &str) -> bool {
    !key.is_empty() && !key.contains('`') && key.len() <= MAX_PATTERN_NAME_BYTES
}

/// One map entry as the scalar (or collection) argument its key binds.
fn argument(value: &GraphValue) -> Option<GqlParameterValue> {
    match value {
        GraphValue::Scalar(scalar) => GqlScalarParameter::new(scalar.clone())
            .ok()
            .map(GqlParameterValue::Scalar),
        GraphValue::List(values) => GqlListParameter::new(values.to_vec())
            .ok()
            .map(GqlParameterValue::List),
        GraphValue::Map { keys, values } => {
            GqlMapParameter::new(keys.iter().cloned().zip(values.iter().cloned()).collect())
                .ok()
                .map(GqlParameterValue::Map)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::CanonicalScalar;

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn map(entries: &[(&str, GraphValue)]) -> GqlParameters {
        GqlParameters::new()
            .with_map(
                "m",
                entries
                    .iter()
                    .map(|(key, value)| ((*key).into(), value.clone()))
                    .collect(),
            )
            .unwrap()
    }
    fn names(arguments: &GqlParameters) -> Vec<String> {
        (arguments.parameter_types())
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    #[test]
    fn set_items_and_patterns_splice_one_parameter_per_key() {
        let arguments = map(&[("a", int(1)), ("b", int(2))]);
        for (text, expected) in [
            (
                "MATCH (n) SET n += $m",
                "MATCH (n) SET n += {`a`: $aa, `b`: $ab}",
            ),
            (
                "MATCH (n) SET n = $m RETURN n.a",
                "MATCH (n) SET n = {`a`: $aa, `b`: $ab} RETURN n.a",
            ),
            (
                "MERGE (n:P {id: 1}) ON CREATE SET n += $m ON MATCH SET n.c = 1, n += $m",
                "MERGE (n:P {id: 1}) ON CREATE SET n += {`a`: $aa, `b`: $ab} ON MATCH SET \
                 n.c = 1, n += {`a`: $ac, `b`: $ad}",
            ),
            ("CREATE (n:P $m)", "CREATE (n:P {`a`: $aa, `b`: $ab})"),
            (
                "MATCH (a), (b) MERGE (a)-[r:R $m]->(b)",
                "MATCH (a), (b) MERGE (a)-[r:R {`a`: $aa, `b`: $ab}]->(b)",
            ),
            (
                "OPTIONAL MATCH (n:A|B $m) RETURN n",
                "OPTIONAL MATCH (n:A|B {`a`: $aa, `b`: $ab}) RETURN n",
            ),
        ] {
            let (lowered, derived) = GqlMapArgumentText::lower(text, &arguments).expect(text);
            assert_eq!(lowered.text(), expected);
            let fresh = expected.matches('$').count();
            assert_eq!(names(&derived).len(), fresh, "{text}");
            assert!(derived.get("m").is_none(), "{text}");
            assert_eq!(derived.get("aa"), arguments_entry(1), "{text}");
            assert_eq!(derived.get("ab"), arguments_entry(2), "{text}");
        }
    }

    /// Prepared once, a lowered statement binds each later map with the same
    /// keys, and refuses one with other keys, where a missing key would
    /// assign NULL, or a value that is not a map.
    #[test]
    fn a_later_binding_needs_the_lowered_keys() {
        let (lowered, _) = GqlMapArgumentText::lower(
            "MATCH (n) SET n += $m",
            &map(&[("a", int(1)), ("b", int(2))]),
        )
        .unwrap();
        let later = lowered
            .arguments(&map(&[("a", int(5)), ("b", int(6))]))
            .unwrap();
        assert_eq!(names(&later), ["aa", "ab"]);
        assert_eq!(later.get("aa"), arguments_entry(5));
        assert_eq!(later.get("ab"), arguments_entry(6));
        for other in [map(&[("a", int(5))]), map(&[("a", int(5)), ("c", int(6))])] {
            assert_eq!(
                lowered.arguments(&other).unwrap_err(),
                GqlMapArgumentError::KeysDiffer {
                    parameter: "m".into()
                }
            );
        }
        for other in [
            GqlParameters::new(),
            GqlParameters::new().with_int64("m", 1).unwrap(),
        ] {
            assert_eq!(
                lowered.arguments(&other).unwrap_err(),
                GqlMapArgumentError::ExpectedMap {
                    parameter: "m".into()
                }
            );
        }
        let element = map(&[("a", GraphValue::Vertex(fgdb_types::VId(1))), ("b", int(2))]);
        assert_eq!(
            lowered.arguments(&element).unwrap_err(),
            GqlMapArgumentError::UnsupportedValue {
                parameter: "m".into()
            }
        );
    }

    fn arguments_entry(value: i64) -> Option<GqlParameterValue> {
        Some(GqlParameterValue::Scalar(
            GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap(),
        ))
    }

    #[test]
    fn other_positions_and_other_arguments_bind_as_written() {
        let arguments = map(&[("a", int(1))]);
        for text in [
            "MATCH (n) WHERE n = $m RETURN n",
            "RETURN $m AS m",
            "MATCH (n) SET n.a = $m",
            "CREATE (n {a: $m})",
            "WITH $m AS x MATCH (n) SET n += x",
            "MATCH (n) CALL { WITH n SET n += $m } RETURN n",
            "MATCH (n) SET n += $m.a",
            "MATCH (n) SET n += $other",
        ] {
            assert!(
                GqlMapArgumentText::lower(text, &arguments).is_none(),
                "{text}"
            );
        }
        // A scalar argument at a site is not a map.
        let scalar = GqlParameters::new().with_int64("m", 1).unwrap();
        assert!(GqlMapArgumentText::lower("MATCH (n) SET n += $m", &scalar).is_none());
        // A text the lexer refuses keeps its own diagnostic.
        assert!(GqlMapArgumentText::lower("MATCH (n) SET n += $m 'open", &arguments).is_none());
    }

    #[test]
    fn a_remaining_reference_keeps_its_argument_and_fresh_names_avoid_every_name() {
        let arguments = map(&[("a", int(1))]).with_int64("aa", 7).unwrap();
        let (lowered, derived) = GqlMapArgumentText::lower(
            "MATCH (n) WHERE n.x = $aa SET n += $m, n.c = $m.a",
            &arguments,
        )
        .unwrap();
        assert_eq!(
            lowered.text(),
            "MATCH (n) WHERE n.x = $aa SET n += {`a`: $ab}, n.c = $m.a"
        );
        assert_eq!(names(&derived), ["aa", "ab", "m"]);
        // Keys are delimited, so a keyword or a spaced key stays a name.
        let spaced = map(&[("order", int(1)), ("a b", int(2))]);
        let (lowered, _) = GqlMapArgumentText::lower("CREATE ($m)", &spaced).unwrap();
        assert_eq!(lowered.text(), "CREATE ({`a b`: $aa, `order`: $ab})");
        // An empty map splices as `{}`, the literal it stands for.
        let empty = map(&[]);
        let (lowered, derived) = GqlMapArgumentText::lower("MATCH (n) SET n = $m", &empty).unwrap();
        assert_eq!(lowered.text(), "MATCH (n) SET n = {}");
        assert!(names(&derived).is_empty());
    }

    #[test]
    fn lowered_offsets_report_the_callers_bytes() {
        let text = "MATCH (n) SET n += $m RETURN n.zz";
        let (lowered, _) = GqlMapArgumentText::lower(text, &map(&[("a", int(1))])).unwrap();
        let generated = lowered.text();
        let site = text.find('$').unwrap();
        // Bytes before the site are unchanged; every spliced byte is the `$`.
        assert_eq!(lowered.original_offset(site - 1), site - 1);
        for at in site..site + "{`a`: $aa}".len() {
            assert_eq!(lowered.original_offset(at), site, "{at}");
        }
        let later = generated.find("n.zz").unwrap();
        assert_eq!(lowered.original_offset(later), text.find("n.zz").unwrap());
        let mut error = super::super::GraphPatternTextError {
            offset: later,
            kind: super::super::GraphPatternTextErrorKind::UnknownVariable,
        };
        lowered.restore(&mut error);
        assert_eq!(error.offset, text.find("n.zz").unwrap());
    }
}
