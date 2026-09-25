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
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                let name =
                    previous.is_some_and(|previous| previous.punct(b'.') || previous.word("AS"));
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
