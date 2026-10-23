//! Native MATCH continuations inside one set-expression leaf.
//!
//! The ordinary parser owns syntax and the ordinary relational engine owns
//! execution. This phase object only connects their existing typed boundaries.

use super::*;
use crate::row_join::{RowJoinKind, RowJoinSpec};

#[derive(Clone)]
pub(crate) struct BoundContinuation {
    pub(crate) input: BoundSetTextInput,
    pub(crate) join: RowJoinSpec,
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

/// The offset of the WHERE keyword of a required MATCH over imported rows (a
/// top-level MATCH after a top-level WITH or a leading UNWIND, not OPTIONAL)
/// whose predicate contains byte `offset`, if any. unresolved_read_input
/// re-reads such a WHERE after an implicit `WITH *` when the graph predicate
/// refused it.
pub(crate) fn continuation_where_at(tokens: &[TextToken<'_>], offset: usize) -> Option<usize> {
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
                        && !previous.is_some_and(|previous| previous.word("OPTIONAL"));
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
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.first.parameter_schema()
    }

    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        if self.continuations.is_empty() {
            // Existing single-source templates remain byte-identical.
            self.first.append_template_transcript(bytes);
            return;
        }
        // Non-equality ON predicates are not derivable from the old equality
        // correlation list. Give them a separate framed input format and use
        // the ordinary relational join transcript, including exact operands.
        // Equality-only definitions (including Any-domain ON) keep their bytes.
        let theta = self.continuations.iter().any(|part| {
            part.join.predicate().is_some_and(|code| {
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
                part.join.append_template_transcript(bytes);
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
            bytes.push(match part.join.kind() {
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
            for types in [part.join.left_types(), part.join.right_types()] {
                bytes.extend_from_slice(&(types.len() as u64).to_be_bytes());
                for kind in types {
                    bytes.push(set_column_type_tag(*kind));
                }
            }
            part.input.append_template_transcript(bytes);
        }
    }
}
