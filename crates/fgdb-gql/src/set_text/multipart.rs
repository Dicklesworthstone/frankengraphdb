//! Native MATCH continuations inside one set-expression leaf.
//!
//! The ordinary parser owns syntax and the ordinary relational engine owns
//! execution. This phase object only connects their existing typed boundaries.

use super::*;
use crate::row_join::{RowJoinKind, RowJoinSpec};

#[derive(Clone)]
pub(crate) struct BoundContinuation {
    pub(crate) input: BoundSetTextInput,
    pub(crate) operation: BoundContinuationOperation,
}

#[derive(Clone)]
pub(crate) enum BoundContinuationOperation {
    Match(RowJoinSpec),
    /// A row-scope existence predicate. Its graph witnesses remain inside
    /// an Apply body; the completed outer row keeps its order and multiplicity.
    /// Some(negated) filters directly; None appends a private Boolean value
    /// for a compound WHERE expression.
    Exists {
        pattern: Box<BoundSetTextInput>,
        join: RowJoinSpec,
        negated: Option<bool>,
    },
    Apply {
        body: Box<BoundReadInput>,
        imports: Vec<usize>,
        /// An OPTIONAL MATCH whose WHERE reads the incoming row: a row the
        /// body leaves empty is null-extended (PreparedGraphSet::optional_apply).
        optional: bool,
    },
}

#[derive(Clone)]
pub(crate) struct BoundReadInput {
    pub(crate) first: BoundSetTextInput,
    pub(crate) continuations: Vec<BoundContinuation>,
}

// A routing decision over the already admitted shared lexer tokens, not a
// second lexer or a fallback after a failed parse. Nested EXISTS/MATCH bodies,
// strings, property names and STARTS/ENDS WITH do not start a query part.
pub(crate) fn has_continuation(tokens: &[TextToken<'_>]) -> bool {
    let mut depth = 0_usize;
    let mut with = false;
    // A leading UNWIND joins a MATCH as the correlated root, an inner join
    // only. OPTIONAL MATCH directly after the unwound rows is instead a Left
    // continuation of them: one NULL-extended row per unmatched value.
    let leading_unwind = tokens.first().is_some_and(|token| token.word("UNWIND"));
    // A leading OPTIONAL MATCH is a Left continuation of the unit relation:
    // its witnesses, or exactly one NULL-extended row.
    if tokens.first().is_some_and(|token| token.word("OPTIONAL"))
        && tokens.get(1).is_some_and(|token| token.word("MATCH"))
    {
        return true;
    }
    let mut matched = false;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                let name =
                    previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS"));
                if !name
                    && token.word("CALL")
                    && tokens
                        .get(index + 1)
                        .is_some_and(|next| next.punct(b'{') || next.punct(b'('))
                {
                    return true;
                }
                if !name && token.word("MATCH") && !previous.is_some_and(|p| p.word("OPTIONAL")) {
                    matched = true;
                }
                if leading_unwind
                    && !matched
                    && !name
                    && token.word("OPTIONAL")
                    && tokens.get(index + 1).is_some_and(|next| next.word("MATCH"))
                {
                    return true;
                }
                if !name
                    && token.word("WITH")
                    && !previous
                        .is_some_and(|previous| previous.word("STARTS") || previous.word("ENDS"))
                {
                    with = true;
                }
                if with && !name && token.word("MATCH") {
                    let next = tokens.get(index + 1);
                    let pattern = next.is_some_and(|next| {
                        next.punct(b'(')
                            || next.word("ALL")
                            || next.word("ANY")
                            || next.word("SHORTEST")
                            || next.word("TRAIL")
                            || next.word("ACYCLIC")
                            || next.word("SIMPLE")
                            || next.word("WALK")
                            || tokens.get(index + 2).is_some_and(|next| next.punct(b'='))
                    });
                    if pattern {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Candidate row WHERE positions. The shared graph lexer must still prove a
/// graph-predicate introducer before any facade routing changes. Scalar NOT,
/// parenthesized arithmetic, strings and nested comprehension WHEREs do not
/// become multipart statements merely because they contain a WHERE token.
pub(crate) fn row_where_offsets(tokens: &[TextToken<'_>]) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut depth = 0_usize;
    let mut row = false;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS")) {
                    continue;
                }
                if token.word("MATCH") {
                    row = false;
                } else if (token.word("WITH") || token.word("UNWIND"))
                    && !previous
                        .is_some_and(|previous| previous.word("STARTS") || previous.word("ENDS"))
                {
                    row = true;
                } else if row && token.word("WHERE") {
                    positions.push(token.at);
                }
            }
            _ => {}
        }
    }
    positions
}

/// The offset of the WHERE keyword of a required MATCH over imported rows (a
/// top-level MATCH after a top-level WITH or a leading UNWIND, not OPTIONAL)
/// whose predicate contains byte `offset`, if any. unresolved_read_input
/// re-reads such a WHERE after an implicit `WITH *` when the graph predicate
/// refused it.
pub(crate) fn continuation_where_at(tokens: &[TextToken<'_>], offset: usize) -> Option<usize> {
    imported_where_at(tokens, offset, false)
}

/// As [`continuation_where_at`] for a top-level OPTIONAL MATCH. Its WHERE,
/// when the graph predicate refused it and no implicit `WITH *` placed it,
/// is re-read inside an optional Apply, before the clause's null extension
/// (multipart_optional_apply).
pub(crate) fn optional_where_at(tokens: &[TextToken<'_>], offset: usize) -> Option<usize> {
    imported_where_at(tokens, offset, true)
}

fn imported_where_at(tokens: &[TextToken<'_>], offset: usize, optional: bool) -> Option<usize> {
    const CLAUSES: [&str; 9] = [
        "WITH", "RETURN", "MATCH", "OPTIONAL", "UNWIND", "UNION", "CALL", "ORDER", "SKIP",
    ];
    let mut depth = 0_usize;
    let mut with = tokens.first().is_some_and(|token| token.word("UNWIND"));
    let mut eligible = false;
    let mut predicate: Option<usize> = None;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS")) {
                    continue;
                }
                let operator = token.word("WITH")
                    && previous
                        .is_some_and(|previous| previous.word("STARTS") || previous.word("ENDS"));
                if !operator && CLAUSES.iter().any(|clause| token.word(clause)) {
                    if let Some(at) = predicate.take()
                        && (at..token.at).contains(&offset)
                    {
                        return Some(at);
                    }
                    eligible = with
                        && token.word("MATCH")
                        && previous.is_some_and(|previous| previous.word("OPTIONAL")) == optional;
                    with |= token.word("WITH");
                } else if eligible && token.word("WHERE") {
                    predicate = Some(token.at);
                    eligible = false;
                }
            }
            _ => {}
        }
    }
    predicate.filter(|&at| at <= offset)
}

/// The offset of the WHERE keyword of a top-level required MATCH whose
/// predicate contains byte `offset` and an existence predicate: `EXISTS`
/// before `{` or `(`, or a node pattern's `)` before an edge introducer.
/// unresolved_read_input re-reads such a WHERE after an implicit `WITH *`
/// when the graph predicate refused it (an existence predicate under OR, for
/// one): the row filter there reads each existence predicate from a hidden
/// column of that boundary (hoist_existence_reads). Lookahead only.
pub(crate) fn existence_where_at(tokens: &[TextToken<'_>], offset: usize) -> Option<usize> {
    const CLAUSES: [&str; 9] = [
        "WITH", "RETURN", "MATCH", "OPTIONAL", "UNWIND", "UNION", "CALL", "ORDER", "SKIP",
    ];
    let mut depth = 0_usize;
    let mut eligible = false;
    // The open WHERE's offset, and whether its predicate reads existence.
    let mut predicate: Option<(usize, bool)> = None;
    for (index, token) in tokens.iter().enumerate() {
        if let Some((_, existence)) = predicate.as_mut() {
            let next = tokens.get(index + 1);
            let after = tokens.get(index + 2);
            *existence |= (token.word("EXISTS")
                && next.is_some_and(|next| next.punct(b'{') || next.punct(b'(')))
                || (token.punct(b')')
                    && next.is_some_and(|next| next.punct(b'-'))
                    && after.is_some_and(|after| {
                        after.punct(b'[') || after.punct(b'-') || after.punct(b'>')
                    }))
                || (token.punct(b')')
                    && next.is_some_and(|next| next.punct(b'<'))
                    && after.is_some_and(|after| after.punct(b'-')));
        }
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS")) {
                    continue;
                }
                let operator = token.word("WITH")
                    && previous
                        .is_some_and(|previous| previous.word("STARTS") || previous.word("ENDS"));
                if !operator && CLAUSES.iter().any(|clause| token.word(clause)) {
                    if let Some((at, true)) = predicate.take()
                        && (at..token.at).contains(&offset)
                    {
                        return Some(at);
                    }
                    eligible = token.word("MATCH")
                        && !previous.is_some_and(|previous| previous.word("OPTIONAL"));
                } else if eligible && token.word("WHERE") {
                    predicate = Some((token.at, false));
                    eligible = false;
                }
            }
            _ => {}
        }
    }
    predicate
        .filter(|&(at, existence)| existence && at <= offset)
        .map(|(at, _)| at)
}

/// The offset of a top-level MATCH or OPTIONAL MATCH clause that contains
/// byte `offset` and directly follows another MATCH or OPTIONAL MATCH clause,
/// if any: the clause unresolved_read_input re-reads after an implicit
/// `WITH *` when it read a name its child scope cannot see. An OPTIONAL
/// clause starts at its OPTIONAL keyword.
pub(crate) fn implicit_with_at(tokens: &[TextToken<'_>], offset: usize) -> Option<usize> {
    const CLAUSES: [&str; 9] = [
        "WITH", "RETURN", "MATCH", "OPTIONAL", "UNWIND", "UNION", "CALL", "ORDER", "SKIP",
    ];
    let mut depth = 0_usize;
    // The open clause's start and whether it follows a MATCH clause.
    let mut clause: Option<(usize, bool)> = None;
    let mut chain = false;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS")) {
                    continue;
                }
                let operator = token.word("WITH")
                    && previous
                        .is_some_and(|previous| previous.word("STARTS") || previous.word("ENDS"));
                // OPTIONAL MATCH is one clause, opened at OPTIONAL.
                let continued = token.word("MATCH")
                    && previous.is_some_and(|previous| previous.word("OPTIONAL"));
                if operator || continued || !CLAUSES.iter().any(|word| token.word(word)) {
                    continue;
                }
                if let Some((start, true)) = clause
                    && (start..token.at).contains(&offset)
                {
                    return Some(start);
                }
                if token.word("MATCH") || token.word("OPTIONAL") {
                    clause = Some((token.at, chain));
                    chain = true;
                } else {
                    clause = None;
                    chain = false;
                }
            }
            _ => {}
        }
    }
    clause.and_then(|(start, later)| (later && start <= offset).then_some(start))
}

/// Whether the statement starts with MATCH and a top-level MATCH or
/// OPTIONAL MATCH clause follows its first top-level WHERE, before any
/// RETURN or WITH: the only statements whose root WHERE, deferred to the
/// row filter, can start a multipart read (Parser::deferred_where_continues).
pub(crate) fn where_then_match(tokens: &[TextToken<'_>]) -> bool {
    if !tokens.first().is_some_and(|token| token.word("MATCH")) {
        return false;
    }
    let mut depth = 0_usize;
    let mut filtered = false;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS")) {
                    continue;
                }
                if token.word("RETURN")
                    || (token.word("WITH")
                        && !previous.is_some_and(|previous| {
                            previous.word("STARTS") || previous.word("ENDS")
                        }))
                {
                    return false;
                }
                if token.word("WHERE") {
                    filtered = true;
                } else if filtered && token.word("MATCH") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a leading UNWIND starts the statement and a top-level MATCH
/// follows its row stages (`UNWIND $ids AS id MATCH (n {id: id}) WITH ...
/// RETURN count(*)`). Where has_continuation is false, a pipeline aggregate
/// then reads the leading pipeline's graph part as a read does: one CrossJoin
/// of the unwound rows and the pattern, filtered by its correlations.
pub(crate) fn leading_rows_then_match(tokens: &[TextToken<'_>]) -> bool {
    if !tokens.first().is_some_and(|token| token.word("UNWIND")) {
        return false;
    }
    let mut depth = 0_usize;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 && token.word("MATCH") => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if !previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS")) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

impl BoundReadInput {
    pub(crate) fn graph_source_count(&self) -> usize {
        usize::from(self.first.selection.is_some())
            + self
                .continuations
                .iter()
                .map(|part| match &part.operation {
                    BoundContinuationOperation::Match(_) => 1,
                    BoundContinuationOperation::Exists { pattern, .. } => {
                        usize::from(pattern.selection.is_some())
                    }
                    BoundContinuationOperation::Apply { body, .. } => body.graph_source_count(),
                })
                .sum::<usize>()
    }

    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.first.parameter_schema()
    }

    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        if self.continuations.is_empty() {
            // Existing single-source templates remain byte-identical.
            self.first.append_template_transcript(bytes);
            return;
        }
        if self.continuations.iter().any(|part| {
            matches!(
                part.operation,
                BoundContinuationOperation::Apply { .. }
                    | BoundContinuationOperation::Exists { .. }
            )
        }) {
            bytes.push(5);
            bytes.extend_from_slice(b"fgdb:gql:apply-continuations:v1\0");
            bytes.extend_from_slice(&(self.continuations.len() as u64).to_be_bytes());
            self.first.append_template_transcript(bytes);
            for part in &self.continuations {
                match &part.operation {
                    BoundContinuationOperation::Match(join) => {
                        bytes.push(0);
                        join.append_template_transcript(bytes);
                    }
                    BoundContinuationOperation::Exists {
                        pattern,
                        join,
                        negated,
                    } => {
                        if let Some(negated) = negated {
                            // Preserve the established standalone-filter identity.
                            bytes.push(3);
                            bytes.push(u8::from(*negated));
                        } else {
                            bytes.push(4);
                        }
                        join.append_template_transcript(bytes);
                        pattern.append_template_transcript(bytes);
                    }
                    BoundContinuationOperation::Apply {
                        body,
                        imports,
                        optional,
                    } => {
                        bytes.push(if *optional { 2 } else { 1 });
                        bytes.extend_from_slice(&(imports.len() as u64).to_be_bytes());
                        for &column in imports {
                            bytes.extend_from_slice(&(column as u64).to_be_bytes());
                        }
                        body.append_template_transcript(bytes);
                    }
                }
                part.input.append_template_transcript(bytes);
            }
            return;
        }
        // Non-equality ON predicates are not derivable from the old equality
        // correlation list. Give them a separate framed input format and use
        // the ordinary relational join transcript, including exact operands.
        // Equality-only definitions (including Any-domain ON) keep their bytes.
        let theta = self.continuations.iter().any(|part| {
            let BoundContinuationOperation::Match(join) = &part.operation else {
                unreachable!("Apply has its own transcript")
            };
            join.predicate().is_some_and(|code| {
                code.iter().any(|op| {
                    matches!(op, crate::GraphSetPredicateOp::Compare { comparison, .. }
                        if *comparison != IntegerComparison::Equal)
                })
            })
        });
        if theta {
            bytes.push(3);
            bytes.extend_from_slice(b"fgdb:gql:match-continuations:theta:v1\0");
            bytes.extend_from_slice(&(self.continuations.len() as u64).to_be_bytes());
            self.first.append_template_transcript(bytes);
            for part in &self.continuations {
                let BoundContinuationOperation::Match(join) = &part.operation else {
                    unreachable!("Apply has its own transcript")
                };
                join.append_template_transcript(bytes);
                part.input.append_template_transcript(bytes);
            }
            return;
        }
        // Ordinary input transcripts start with the closed 0/1 selection tag.
        // Tag 2 therefore cannot collide with any existing input definition.
        bytes.push(2);
        bytes.extend_from_slice(b"fgdb:gql:match-continuations:v1\0");
        bytes.extend_from_slice(&(self.continuations.len() as u64).to_be_bytes());
        self.first.append_template_transcript(bytes);
        for part in &self.continuations {
            let BoundContinuationOperation::Match(join) = &part.operation else {
                unreachable!("Apply has its own transcript")
            };
            bytes.push(match join.kind() {
                RowJoinKind::Inner => 0,
                RowJoinKind::Left => 1,
                RowJoinKind::Right => 2,
                RowJoinKind::Full => 3,
                RowJoinKind::Semi => 4,
                RowJoinKind::Anti => 5,
            });
            // The shared input transcript contains the exact correlation
            // columns and all projections. The private join is derived solely
            // from those correlations and these frozen input domains; it
            // cannot contain an arbitrary ON predicate or a caller callback.
            for types in [join.left_types(), join.right_types()] {
                bytes.extend_from_slice(&(types.len() as u64).to_be_bytes());
                for kind in types {
                    bytes.push(set_column_type_tag(*kind));
                }
            }
            part.input.append_template_transcript(bytes);
        }
    }
}
