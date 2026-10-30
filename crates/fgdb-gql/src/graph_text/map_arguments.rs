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
//! A field of a map argument read inside such a pattern, `(n {id: $row.id})`,
//! is spliced as one fresh parameter holding the field's value: a pattern's
//! property map compares only constants, and `$row.id` is one. A missing key
//! reads NULL, as it does anywhere; a field past a value that is not a map is
//! left as written. The fresh name is exactly as long as the access it
//! replaces, `$row_id`, so no byte of the statement moves.
//!
//! The batch form `UNWIND $rows AS row ... SET n += row` assigns each row's
//! map. When every row holds the same keys, `row` there is spliced as the
//! literal map of those keys read from the row, `{`a`: row.a, `b`: row.b}`;
//! a later binding must have them too. Rows with different keys would assign
//! NULL where `+=` leaves a property, so they are left as written, as is an
//! alias another `AS` rebinds.
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
use fgdb_types::CanonicalScalar;
use std::collections::{BTreeMap, BTreeSet};

/// A statement whose map arguments at property-map sites are spliced as
/// literal maps of fresh parameters, one per key, and whose map fields in
/// patterns are spliced as one fresh parameter apiece.
pub struct GqlMapArgumentText {
    text: String,
    edits: Box<[UnwindSourceOffset]>,
    /// Each spliced map, in text order.
    maps: Box<[SplicedMap]>,
    /// Each spliced field, in text order.
    fields: Box<[SplicedField]>,
    /// Each spliced UNWIND row, in text order.
    rows: Box<[SplicedRows]>,
    /// The arguments no remaining token reads.
    spliced: Box<[Box<str>]>,
}

/// One spliced map: its argument and, for every key in order, the parameter
/// the key binds.
struct SplicedMap {
    parameter: Box<str>,
    keys: Box<[(Box<str>, Box<str>)]>,
}

/// One spliced `$parameter.k1.k2`: the keys it reads and the parameter that
/// binds the value they reach.
struct SplicedField {
    parameter: Box<str>,
    path: Box<[Box<str>]>,
    name: Box<str>,
}

/// One spliced `SET n += row` over `UNWIND $parameter AS row`: the keys every
/// row of the list must hold.
struct SplicedRows {
    parameter: Box<str>,
    keys: Box<[Box<str>]>,
}

/// Where a statement's text reads a map argument.
#[derive(Clone, Copy)]
enum Site {
    /// The parameter token standing where a property map does.
    Map(usize),
    /// `$p.k1.k2` in a pattern: the parameter token and the last key's.
    Field(usize, usize),
    /// `SET n += row`: the alias token there, and the list parameter token
    /// of the `UNWIND $p AS row` that binds it.
    Rows { alias: usize, list: usize },
}

impl Site {
    /// The token of the argument the site reads.
    fn parameter(self) -> usize {
        match self {
            Self::Map(parameter) | Self::Field(parameter, _) => parameter,
            Self::Rows { list, .. } => list,
        }
    }
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
    /// The UNWIND argument is not a list of maps with the lowered keys.
    RowsDiffer {
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
            Self::RowsDiffer { parameter } => write!(
                f,
                "${parameter} must be a list of maps with the keys the statement was prepared with"
            ),
            Self::Parameter(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GqlMapArgumentError {}

impl GqlMapArgumentText {
    /// The lowered statement and the arguments that bind it, or `None` when
    /// no site reads a map argument: bind `text` with `arguments` as they are.
    #[must_use]
    pub fn lower(text: &str, arguments: &GqlParameters) -> Option<(Self, GqlParameters)> {
        let lowered = Self::splice(text, arguments, false)?;
        let derived = lowered.arguments(arguments).ok()?;
        Some((lowered, derived))
    }

    /// As `lower`, splicing only the `$p.k` fields in patterns: the sites a
    /// read has (fgdb-8srlm). Neo4j refuses a parameter map in MATCH, so a
    /// read keeps `(n $p)` as written. A field keeps its length, so every
    /// byte of the lowered read is the caller's and no diagnostic moves.
    #[must_use]
    pub fn lower_fields(text: &str, arguments: &GqlParameters) -> Option<(Self, GqlParameters)> {
        let lowered = Self::splice(text, arguments, true)?;
        let derived = lowered.arguments(arguments).ok()?;
        Some((lowered, derived))
    }

    fn splice(text: &str, arguments: &GqlParameters, fields_only: bool) -> Option<Self> {
        let tokens = tokens(text)?;
        // Each site whose argument fits it: a map site needs a map with
        // delimitable keys, a field a map whose keys reach a value a
        // parameter carries, a rows site a list of maps with one key set.
        let sites: Vec<(Site, &str, GqlParameterValue)> = property_map_sites(text, &tokens)
            .into_iter()
            .filter(|site| !fields_only || matches!(site, Site::Field(..)))
            .filter_map(|site| {
                let TokenKind::Parameter(name) = tokens[site.parameter()].kind else {
                    return None;
                };
                let bound = arguments.get(name)?;
                let admitted = match (site, &bound) {
                    (Site::Map(_), GqlParameterValue::Map(map)) => {
                        map.entries().all(|(key, _)| delimitable(key))
                    }
                    (Site::Field(parameter, last), GqlParameterValue::Map(map)) => {
                        field(map.value(), &path(&tokens, parameter, last))
                            .ok()
                            .and_then(|value| argument(&value))
                            .is_some()
                    }
                    (Site::Rows { .. }, GqlParameterValue::List(list)) => {
                        row_keys(list.value()).is_some()
                    }
                    _ => false,
                };
                admitted.then_some((site, name, bound))
            })
            .collect();
        if sites.is_empty() {
            return None;
        }

        // An argument leaves the binding only when every token naming it
        // was spliced; `SET n += $m RETURN $m.k` still reads `$m`, and a
        // spliced UNWIND row still reads its list.
        let mut occurrences: BTreeMap<&str, usize> = BTreeMap::new();
        for token in &tokens {
            if let TokenKind::Parameter(name) = token.kind {
                *occurrences.entry(name).or_default() += 1;
            }
        }
        for (site, name, _) in &sites {
            if let (Site::Map(_) | Site::Field(..), Some(count)) = (site, occurrences.get_mut(name))
            {
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
        let mut maps = Vec::new();
        let mut fields = Vec::new();
        let mut rows = Vec::new();
        let mut cursor = 0;
        let mut copied = 0;
        for (site, name, bound) in &sites {
            let (start, end) = site_span(&tokens, *site);
            lowered.push_str(&text[copied..start]);
            let generated = lowered.len();
            match (*site, bound) {
                (Site::Map(_), GqlParameterValue::Map(map)) => {
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
                    maps.push(SplicedMap {
                        parameter: (*name).into(),
                        keys: keys.into_boxed_slice(),
                    });
                }
                (Site::Field(parameter, last), _) => {
                    let fresh = same_length_name(&text[start + 1..end], &mut reserved)?;
                    lowered.push('$');
                    lowered.push_str(&fresh);
                    fields.push(SplicedField {
                        parameter: (*name).into(),
                        path: path(&tokens, parameter, last),
                        name: fresh.into(),
                    });
                }
                (Site::Rows { .. }, GqlParameterValue::List(list)) => {
                    let alias = &text[start..end];
                    let keys = row_keys(list.value())?;
                    lowered.push('{');
                    for (index, key) in keys.iter().enumerate() {
                        if index != 0 {
                            lowered.push_str(", ");
                        }
                        lowered.push('`');
                        lowered.push_str(key);
                        lowered.push_str("`: ");
                        lowered.push_str(alias);
                        lowered.push('.');
                        lowered.push_str(key);
                    }
                    lowered.push('}');
                    rows.push(SplicedRows {
                        parameter: (*name).into(),
                        keys,
                    });
                }
                _ => unreachable!("sites are admitted with their argument's kind"),
            }
            edits.push(UnwindSourceOffset {
                generated: generated..lowered.len(),
                original: start..end,
            });
            copied = end;
        }
        lowered.push_str(&text[copied..]);
        Some(Self {
            text: lowered,
            edits: edits.into_boxed_slice(),
            maps: maps.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            rows: rows.into_boxed_slice(),
            spliced,
        })
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Restore the caller-facing requirements of a `lower_fields` template.
    /// Generated scalar names are implementation details: callers supply the
    /// original map, even when several fields or remaining value expressions
    /// read it. The native template still checks each derived field's type.
    #[must_use]
    pub fn field_parameter_schema(
        &self,
        native: &[crate::GqlParameterSpec],
    ) -> Vec<crate::GqlParameterSpec> {
        let mut schema: BTreeMap<String, crate::GqlParameterSpec> = BTreeMap::new();
        for parameter in native {
            let mut parameter = parameter.clone();
            if let Some(field) = self
                .fields
                .iter()
                .find(|field| *field.name == parameter.name)
            {
                parameter.name = field.parameter.to_string();
                parameter.parameter_type = crate::GqlParameterType::Map;
                parameter.requires_positive = false;
            }
            match schema.entry(parameter.name.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(parameter);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().occurrences += parameter.occurrences;
                    entry.get_mut().requires_positive |= parameter.requires_positive;
                }
            }
        }
        schema.into_values().collect()
    }

    /// The arguments that bind the lowered statement for one binding of the
    /// caller's: each argument a remaining token reads, plus one per spliced
    /// key, read from that binding's map, and one per spliced field. A
    /// spliced map's keys must be the lowered ones.
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
        for SplicedField {
            parameter,
            path,
            name,
        } in &self.fields
        {
            let Some(GqlParameterValue::Map(map)) = arguments.get(parameter) else {
                return Err(GqlMapArgumentError::ExpectedMap {
                    parameter: parameter.clone(),
                });
            };
            // `$m.a.b` past a scalar `$m.a` names `m.a` as the non-map.
            let value = field(map.value(), path).map_err(|depth| {
                let mut prefix = parameter.to_string();
                for key in &path[..depth] {
                    prefix.push('.');
                    prefix.push_str(key);
                }
                GqlMapArgumentError::ExpectedMap {
                    parameter: prefix.into(),
                }
            })?;
            let value = argument(&value).ok_or_else(|| GqlMapArgumentError::UnsupportedValue {
                parameter: parameter.clone(),
            })?;
            derived
                .insert(&**name, value)
                .map_err(GqlMapArgumentError::Parameter)?;
        }
        // A spliced row reads the list it unwinds, which stays an argument;
        // each binding's rows, if it has any, must hold the lowered keys.
        for SplicedRows { parameter, keys } in &self.rows {
            let fits = match arguments.get(parameter) {
                Some(GqlParameterValue::List(list)) => {
                    list.value().as_list().is_some_and(<[_]>::is_empty)
                        || row_keys(list.value()).as_deref() == Some(&**keys)
                }
                _ => false,
            };
            if !fits {
                return Err(GqlMapArgumentError::RowsDiffer {
                    parameter: parameter.clone(),
                });
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
    "LIMIT", "CALL", "UNION", "YIELD", "FOREACH", "OPTIONAL", "FINISH", "FILTER", "LET", "NEXT",
    "USE", "FOR",
];

fn is_clause(word: &str) -> bool {
    let named = |words: &[&str]| words.iter().any(|clause| word.eq_ignore_ascii_case(clause));
    word.eq_ignore_ascii_case("SET") || named(PATTERN_CLAUSES) || named(OTHER_CLAUSES)
}

/// The sites reading a map argument, in text order: parameter tokens
/// standing where a property map does, `$p.k` inside a pattern, and a SET
/// item assigning an UNWIND row. Only top-level clauses count: a subquery's
/// braces hide its own.
fn property_map_sites(text: &str, tokens: &[Token<'_>]) -> Vec<Site> {
    let rows = unwound_rows(text, tokens);
    let mut sites = Vec::new();
    let mut clause = Clause::Other;
    let mut depth = 0_usize;
    let mut item = None;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => {
                if depth == 0 && clause == Clause::Pattern && open != b'{' {
                    sites.extend(pattern_site(tokens, index, open).map(Site::Map));
                }
                depth += 1;
            }
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            // Inside a pattern, `$p.k` is a constant wherever it stands.
            TokenKind::Parameter(_) if depth > 0 && clause == Clause::Pattern => {
                sites.extend(field_site(text, tokens, index));
            }
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
                sites.extend(set_site(text, tokens, index, &rows));
            }
            _ => {}
        }
    }
    sites
}

/// `name += $p` or `name = $p`, ending the SET item that starts at `start`,
/// or the same over an UNWIND row alias in `rows`.
fn set_site(
    text: &str,
    tokens: &[Token<'_>],
    start: usize,
    rows: &[(&str, usize)],
) -> Option<Site> {
    let mut next = start + 1;
    if matches!(tokens.get(next)?.kind, TokenKind::Punct(b'+')) {
        next += 1;
    }
    if !matches!(tokens.get(next)?.kind, TokenKind::Punct(b'=')) {
        return None;
    }
    let operand = next + 1;
    let site = match tokens.get(operand)?.kind {
        TokenKind::Parameter(_) => Site::Map(operand),
        TokenKind::Word(alias) if plain(text, &tokens[operand]) => {
            let &(_, list) = rows.iter().find(|(row, _)| *row == alias)?;
            Site::Rows {
                alias: operand,
                list,
            }
        }
        _ => return None,
    };
    match tokens.get(operand + 1)?.kind {
        TokenKind::End | TokenKind::Punct(b',' | b';') => Some(site),
        TokenKind::Word(word) if is_clause(word) => Some(site),
        _ => None,
    }
}

/// Each plain alias a top-level `UNWIND $p AS row` binds and no other `AS`
/// rebinds, with the token of its list parameter.
fn unwound_rows<'t>(text: &str, tokens: &[Token<'t>]) -> Vec<(&'t str, usize)> {
    let word = |at: usize, expected: &str| {
        matches!(tokens.get(at).map(|token| token.kind),
            Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case(expected))
    };
    let mut depth = 0_usize;
    let mut rows = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            TokenKind::Word(_) if depth == 0 && word(index, "UNWIND") && word(index + 2, "AS") => {
                if let (Some(TokenKind::Parameter(_)), Some(TokenKind::Word(alias))) = (
                    tokens.get(index + 1).map(|token| token.kind),
                    tokens.get(index + 3).map(|token| token.kind),
                ) && plain(text, &tokens[index + 3])
                {
                    rows.push((alias, index + 1));
                }
            }
            _ => {}
        }
    }
    rows.retain(|&(alias, _)| {
        let bindings = (tokens.windows(2))
            .filter(|pair| {
                matches!(pair[0].kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"))
                    && matches!(pair[1].kind, TokenKind::Word(name) if name == alias)
            })
            .count();
        bindings == 1
    });
    rows
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

/// `$p.k1.k2` starting at the parameter token: each key a plain name, with
/// nothing between the tokens, so that `p_k1_k2` is a parameter name the
/// lexer reads.
fn field_site(text: &str, tokens: &[Token<'_>], parameter: usize) -> Option<Site> {
    let TokenKind::Parameter(name) = tokens[parameter].kind else {
        return None;
    };
    if !name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut end = tokens[parameter].at + 1 + name.len();
    let mut length = name.len();
    let mut last = None;
    let mut at = parameter + 1;
    while matches!(tokens.get(at)?.kind, TokenKind::Punct(b'.')) && tokens[at].at == end {
        let key = tokens.get(at + 1)?;
        let (TokenKind::Word(name) | TokenKind::DelimitedKeyword(name)) = key.kind else {
            break;
        };
        if key.at != end + 1 || !plain(text, key) {
            break;
        }
        end = key.at + name.len();
        length += 1 + name.len();
        last = Some(at + 1);
        at += 2;
    }
    (length <= MAX_PATTERN_NAME_BYTES)
        .then_some(last)
        .flatten()
        .map(|last| Site::Field(parameter, last))
}

/// A name token spelled as its name, not delimited: `` `a` `` lexes as the
/// word `a` at its backtick, and spans more bytes than its name.
fn plain(text: &str, token: &Token<'_>) -> bool {
    !text[token.at..].starts_with('`')
}

/// The bytes a site's tokens span in the statement; its names are plain.
fn site_span(tokens: &[Token<'_>], site: Site) -> (usize, usize) {
    let (first, last) = match site {
        Site::Map(parameter) => (parameter, parameter),
        Site::Field(parameter, last) => (parameter, last),
        Site::Rows { alias, .. } => (alias, alias),
    };
    let end = match tokens[last].kind {
        TokenKind::Parameter(name) => tokens[last].at + 1 + name.len(),
        TokenKind::Word(name) | TokenKind::DelimitedKeyword(name) => tokens[last].at + name.len(),
        _ => unreachable!("a site ends at a parameter or a name"),
    };
    (tokens[first].at, end)
}

/// A fresh parameter name exactly as long as the access `p.k1.k2` it
/// replaces: `p_k1_k2`, or the first free spelling that changes its later
/// bytes. No byte after the access moves.
fn same_length_name(access: &str, reserved: &mut BTreeSet<String>) -> Option<String> {
    let mut name: Vec<u8> = access
        .bytes()
        .map(|byte| if byte == b'.' { b'_' } else { byte })
        .collect();
    let mut fresh = |name: &[u8]| {
        let name = String::from_utf8(name.to_vec()).ok()?;
        reserved.insert(name.clone()).then_some(name)
    };
    if let Some(name) = fresh(&name) {
        return Some(name);
    }
    for at in (1..name.len()).rev() {
        let kept = name[at];
        for byte in (b'a'..=b'z').chain(b'0'..=b'9') {
            name[at] = byte;
            if let Some(name) = fresh(&name) {
                return Some(name);
            }
        }
        name[at] = kept;
    }
    None
}

/// The one key set every row of `rows` holds, each key a name `row.key`
/// reads as written; `None` for anything but a list of such maps.
fn row_keys(rows: &GraphValue) -> Option<Box<[Box<str>]>> {
    let mut keys: Option<&[Box<str>]> = None;
    for row in rows.as_list()? {
        let (held, _) = row.as_map()?;
        match keys {
            Some(keys) if keys != held => return None,
            _ => keys = Some(held),
        }
    }
    let keys = keys.unwrap_or_default();
    keys.iter().all(|key| plain_name(key)).then(|| keys.into())
}

/// A name the lexer reads as one word: `[A-Za-z_][A-Za-z0-9_]*`.
fn plain_name(name: &str) -> bool {
    name.len() <= MAX_PATTERN_NAME_BYTES
        && name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The keys of the field site `$p.k1.k2` ending at `last`.
fn path(tokens: &[Token<'_>], parameter: usize, last: usize) -> Box<[Box<str>]> {
    (tokens[parameter + 2..=last].iter().step_by(2))
        .filter_map(|token| match token.kind {
            TokenKind::Word(key) | TokenKind::DelimitedKeyword(key) => Some(key.into()),
            _ => None,
        })
        .collect()
}

/// The value the keys `path` reach in `map`: NULL for a missing key or past
/// a NULL, as `$m.k` reads anywhere. Past any other value that is not a map,
/// the number of keys read before it.
fn field(map: &GraphValue, path: &[Box<str>]) -> Result<GraphValue, usize> {
    let mut value = map;
    for (depth, key) in path.iter().enumerate() {
        if matches!(value, GraphValue::Scalar(CanonicalScalar::Null)) {
            break;
        }
        let (keys, values) = value.as_map().ok_or(depth)?;
        match keys.binary_search_by(|stored| stored.as_bytes().cmp(key.as_bytes())) {
            Ok(at) => value = &values[at],
            Err(_) => return Ok(GraphValue::Scalar(CanonicalScalar::Null)),
        }
    }
    Ok(value.clone())
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

    /// `$m.k` inside a pattern is one fresh parameter holding the field,
    /// named as long as the access: a nested path reads through maps, and a
    /// missing key or a NULL on the way reads NULL. Elsewhere, and past a
    /// value that is not a map, the statement keeps `$m.k`.
    #[test]
    fn a_map_field_in_a_pattern_splices_as_one_parameter() {
        let nested = GraphValue::map(vec![("d".into(), int(4))]).unwrap();
        let arguments = map(&[
            ("a", int(1)),
            ("b", int(2)),
            ("c", nested),
            ("n", GraphValue::Scalar(CanonicalScalar::Null)),
        ]);
        let null = Some(GqlParameterValue::Scalar(
            GqlScalarParameter::new(CanonicalScalar::Null).unwrap(),
        ));
        for (text, expected, values) in [
            (
                "MATCH (n:P {id: $m.a}) SET n.x = 1",
                "MATCH (n:P {id: $m_a}) SET n.x = 1",
                vec![("m_a", arguments_entry(1))],
            ),
            (
                "MATCH (a {id: $m.a})-[r:R {w: $m.b}]->(b) DELETE r",
                "MATCH (a {id: $m_a})-[r:R {w: $m_b}]->(b) DELETE r",
                vec![("m_a", arguments_entry(1)), ("m_b", arguments_entry(2))],
            ),
            (
                "MERGE (n:P {id: $m.c.d}) ON CREATE SET n += $m",
                "MERGE (n:P {id: $m_c_d}) ON CREATE SET n += {`a`: $aa, `b`: $ab, `c`: $ac, `n`: $ad}",
                vec![
                    ("m_c_d", arguments_entry(4)),
                    ("aa", arguments_entry(1)),
                    ("ab", arguments_entry(2)),
                ],
            ),
            (
                "MATCH (n {id: $m.zz}), (o {id: $m.n.d}) RETURN n",
                "MATCH (n {id: $m_zz}), (o {id: $m_n_d}) RETURN n",
                vec![("m_zz", null.clone()), ("m_n_d", null.clone())],
            ),
            (
                "CREATE (n:P {id: $m.order, list: [$m.a, 2]})",
                "CREATE (n:P {id: $m_order, list: [$m_a, 2]})",
                vec![("m_order", null.clone()), ("m_a", arguments_entry(1))],
            ),
        ] {
            let (lowered, derived) = GqlMapArgumentText::lower(text, &arguments).expect(text);
            assert_eq!(lowered.text(), expected);
            assert!(derived.get("m").is_none(), "{text}");
            for (name, value) in values {
                assert_eq!(derived.get(name), value, "{text} {name}");
            }
        }
        // A remaining `$m` keeps its argument, and a taken name is avoided
        // at the same length.
        let (lowered, derived) =
            GqlMapArgumentText::lower("MATCH (n {id: $m.a}) SET n.b = $m.b", &arguments).unwrap();
        assert_eq!(lowered.text(), "MATCH (n {id: $m_a}) SET n.b = $m.b");
        assert_eq!(names(&derived), ["m", "m_a"]);
        let taken = arguments.clone().with_int64("m_a", 7).unwrap();
        let (lowered, derived) =
            GqlMapArgumentText::lower("MATCH (n {id: $m.a}) WHERE n.x = $m_a", &taken).unwrap();
        assert_eq!(lowered.text(), "MATCH (n {id: $m_b}) WHERE n.x = $m_a");
        assert_eq!(names(&derived), ["m_a", "m_b"]);
        for text in [
            "MATCH (n) WHERE n.id = $m.a RETURN n",
            "MATCH (n) SET n.x = $m.a",
            "MATCH (n) RETURN $m.a",
            "MATCH (n {id: $m.a.d}) RETURN n",
            "MATCH (n {id: $m.`a`}) RETURN n",
            "MATCH (n {id: $m . a}) RETURN n",
            "MATCH (n {id: $other.a}) RETURN n",
        ] {
            assert!(
                GqlMapArgumentText::lower(text, &arguments).is_none(),
                "{text}"
            );
        }
    }

    /// A read lowers its pattern fields and nothing else: a parameter map in
    /// MATCH, which Neo4j refuses, stays as written (fgdb-8srlm).
    #[test]
    fn a_read_lowers_only_its_pattern_fields() {
        let arguments = map(&[("a", int(1))]);
        let (lowered, derived) =
            GqlMapArgumentText::lower_fields("MATCH (n {id: $m.a}) RETURN n", &arguments).unwrap();
        assert_eq!(lowered.text(), "MATCH (n {id: $m_a}) RETURN n");
        assert_eq!(names(&derived), ["m_a"]);
        let (lowered, derived) =
            GqlMapArgumentText::lower_fields("MATCH (n:L $m), (o {id: $m.a}) RETURN n", &arguments)
                .unwrap();
        assert_eq!(lowered.text(), "MATCH (n:L $m), (o {id: $m_a}) RETURN n");
        assert_eq!(names(&derived), ["m", "m_a"]);
        for text in ["MATCH (n:L $m) RETURN n", "MATCH (n) SET n += $m"] {
            assert!(
                GqlMapArgumentText::lower_fields(text, &arguments).is_none(),
                "{text}"
            );
        }
    }

    /// Each binding reads its own map's field; a key it lacks is NULL, and
    /// a value that is not a map refuses, naming the access that is not.
    #[test]
    fn a_later_binding_reads_its_own_field() {
        let text = "MATCH (n {id: $m.c.d}) RETURN n";
        let nested = |value| GraphValue::map(vec![("d".into(), value)]).unwrap();
        let (lowered, _) = GqlMapArgumentText::lower(text, &map(&[("c", nested(int(1)))])).unwrap();
        let later = lowered.arguments(&map(&[("c", nested(int(5)))])).unwrap();
        assert_eq!(later.get("m_c_d"), arguments_entry(5));
        let missing = lowered.arguments(&map(&[("x", int(5))])).unwrap();
        assert_eq!(
            missing.get("m_c_d"),
            Some(GqlParameterValue::Scalar(
                GqlScalarParameter::new(CanonicalScalar::Null).unwrap()
            ))
        );
        for (other, parameter) in [
            (GqlParameters::new().with_int64("m", 1).unwrap(), "m"),
            (map(&[("c", int(1))]), "m.c"),
        ] {
            assert_eq!(
                lowered.arguments(&other).unwrap_err(),
                GqlMapArgumentError::ExpectedMap {
                    parameter: parameter.into()
                }
            );
        }
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

        // A field keeps its length: no later byte moves.
        let text = "MATCH (n {id: $m.a}) RETURN n.zz";
        let (lowered, _) = GqlMapArgumentText::lower(text, &map(&[("a", int(1))])).unwrap();
        assert_eq!(lowered.text().len(), text.len());
        let site = text.find('$').unwrap();
        for at in (0..site).chain(site + "$m.a".len()..text.len()) {
            assert_eq!(lowered.original_offset(at), at, "{at}");
        }
    }

    fn rows(rows: &[&[(&str, GraphValue)]]) -> GqlParameters {
        let rows = (rows.iter())
            .map(|row| {
                let entries = row
                    .iter()
                    .map(|(key, value)| ((*key).into(), value.clone()));
                GraphValue::map(entries.collect()).unwrap()
            })
            .collect();
        GqlParameters::new().with_list("rows", rows).unwrap()
    }

    /// `UNWIND $rows AS row ... SET n += row` splices `row` as the literal
    /// map of the keys every row holds, read from the row, and the list stays
    /// the argument. Rows with different keys, a key `row.key` cannot read,
    /// or a rebound alias leave the statement as written.
    #[test]
    fn an_unwind_row_assigned_whole_splices_as_its_keys() {
        let batch = rows(&[
            &[("age", int(1)), ("name", int(2))],
            &[("age", int(3)), ("name", int(4))],
        ]);
        for (text, expected) in [
            (
                "UNWIND $rows AS row CREATE (n:P) SET n += row",
                "UNWIND $rows AS row CREATE (n:P) SET n += {`age`: row.age, `name`: row.name}",
            ),
            (
                "UNWIND $rows AS r MATCH (n {name: r.name}) SET n = r, n.seen = true RETURN n",
                "UNWIND $rows AS r MATCH (n {name: r.name}) SET n = {`age`: r.age, `name`: \
                 r.name}, n.seen = true RETURN n",
            ),
        ] {
            let (lowered, derived) = GqlMapArgumentText::lower(text, &batch).expect(text);
            assert_eq!(lowered.text(), expected);
            assert_eq!(names(&derived), ["rows"], "{text}");
        }
        let (lowered, _) =
            GqlMapArgumentText::lower("UNWIND $rows AS row CREATE (n) SET n += row", &rows(&[]))
                .unwrap();
        assert_eq!(lowered.text(), "UNWIND $rows AS row CREATE (n) SET n += {}");
        let text = "UNWIND $rows AS row CREATE (n) SET n += row";
        for other in [
            rows(&[&[("age", int(1))], &[("name", int(2))]]),
            rows(&[&[("a b", int(1))]]),
            GqlParameters::new().with_int64("rows", 1).unwrap(),
        ] {
            assert!(GqlMapArgumentText::lower(text, &other).is_none());
        }
        for text in [
            "UNWIND $rows AS row WITH row.age AS row CREATE (n) SET n += row",
            "UNWIND $rows AS `row` CREATE (n) SET n += `row`",
            "UNWIND $rows AS row CREATE (n) SET n += row.props",
            "UNWIND [1] AS row CREATE (n) SET n += row",
            "MATCH (n) CALL { UNWIND $rows AS row SET n += row } RETURN n",
        ] {
            assert!(GqlMapArgumentText::lower(text, &batch).is_none(), "{text}");
        }
    }

    /// Each later batch must hold the lowered keys, or have no rows at all.
    #[test]
    fn a_later_batch_needs_the_lowered_row_keys() {
        let text = "UNWIND $rows AS row CREATE (n) SET n += row";
        let (lowered, _) = GqlMapArgumentText::lower(text, &rows(&[&[("age", int(1))]])).unwrap();
        let error = GqlMapArgumentError::RowsDiffer {
            parameter: "rows".into(),
        };
        assert!(lowered.arguments(&rows(&[&[("age", int(5))]])).is_ok());
        assert!(lowered.arguments(&rows(&[])).is_ok());
        for other in [
            rows(&[&[("name", int(5))]]),
            rows(&[&[("age", int(5))], &[("age", int(6)), ("x", int(7))]]),
            GqlParameters::new().with_int64("rows", 1).unwrap(),
        ] {
            assert_eq!(lowered.arguments(&other).unwrap_err(), error);
        }
    }
}
