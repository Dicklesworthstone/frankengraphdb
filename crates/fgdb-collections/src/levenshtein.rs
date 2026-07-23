//! Bounded byte-level Levenshtein automata for ART product traversal.
//!
//! The state transition is independent of any tree representation: an ART
//! walker advances one state through each compressed-prefix and edge byte,
//! pruning a subtree as soon as [`LevenshteinState::can_match_descendant`]
//! becomes false. The implementation supports the exact `edit <= 2` domain
//! required by the term dictionary and performs no ambient Unicode folding;
//! callers must supply bytes from their pinned text profile.

#![forbid(unsafe_code)]

use core::fmt;

/// Largest edit distance admitted by the term-dictionary contract.
pub const MAX_EDIT_DISTANCE: u8 = 2;

/// Allocation named by a Levenshtein construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LevenshteinAllocation {
    /// Owned pattern bytes.
    Pattern,
    /// One dynamic-programming state row.
    State,
}

/// Checked Levenshtein automaton failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LevenshteinError {
    /// The requested edit bound lies outside the closed `0..=2` domain.
    UnsupportedDistance {
        /// Rejected distance.
        requested: u8,
        /// Largest supported distance.
        maximum: u8,
    },
    /// Pattern bytes exceed the caller-provided ceiling.
    PatternLimitExceeded {
        /// Pattern bytes supplied.
        actual: usize,
        /// Caller-provided byte ceiling.
        limit: usize,
    },
    /// The state-row length cannot be represented.
    StateLengthOverflow,
    /// Reserving bounded owned storage failed.
    AllocationFailed {
        /// Storage being allocated.
        target: LevenshteinAllocation,
        /// Exact bytes or entries requested, according to `target`.
        requested: usize,
    },
    /// A state created by another automaton was supplied.
    StateOwnerMismatch,
    /// The supplied state has an incompatible dynamic-programming row width.
    StateWidthMismatch {
        /// Exact entries required for this pattern.
        expected: usize,
        /// Entries present in the supplied state.
        actual: usize,
    },
}

impl fmt::Display for LevenshteinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnsupportedDistance { requested, maximum } => write!(
                formatter,
                "Levenshtein distance {requested} exceeds supported maximum {maximum}"
            ),
            Self::PatternLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "Levenshtein pattern has {actual} bytes, limit is {limit}"
                )
            }
            Self::StateLengthOverflow => {
                formatter.write_str("Levenshtein state-row length overflows usize")
            }
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} entries for Levenshtein {target:?}"
            ),
            Self::StateOwnerMismatch => {
                formatter.write_str("Levenshtein state belongs to another automaton")
            }
            Self::StateWidthMismatch { expected, actual } => write!(
                formatter,
                "Levenshtein state has {actual} entries, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for LevenshteinError {}

/// Immutable byte-pattern automaton.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LevenshteinAutomaton {
    pattern: Vec<u8>,
    max_edits: u8,
}

impl LevenshteinAutomaton {
    /// Copies a bounded byte pattern and fixes its accepted edit distance.
    pub fn try_new(
        pattern: &[u8],
        max_edits: u8,
        max_pattern_bytes: usize,
    ) -> Result<Self, LevenshteinError> {
        if max_edits > MAX_EDIT_DISTANCE {
            return Err(LevenshteinError::UnsupportedDistance {
                requested: max_edits,
                maximum: MAX_EDIT_DISTANCE,
            });
        }
        if pattern.len() > max_pattern_bytes {
            return Err(LevenshteinError::PatternLimitExceeded {
                actual: pattern.len(),
                limit: max_pattern_bytes,
            });
        }

        let mut owned = Vec::new();
        owned
            .try_reserve_exact(pattern.len())
            .map_err(|_| LevenshteinError::AllocationFailed {
                target: LevenshteinAllocation::Pattern,
                requested: pattern.len(),
            })?;
        owned.extend_from_slice(pattern);
        Ok(Self {
            pattern: owned,
            max_edits,
        })
    }

    /// Returns the exact byte pattern.
    #[must_use]
    pub fn pattern(&self) -> &[u8] {
        &self.pattern
    }

    /// Returns the fixed edit-distance ceiling.
    #[must_use]
    pub const fn max_edits(&self) -> u8 {
        self.max_edits
    }

    /// Constructs the state before any candidate bytes are consumed.
    pub fn initial_state(&self) -> Result<LevenshteinState<'_>, LevenshteinError> {
        let state_len = self
            .pattern
            .len()
            .checked_add(1)
            .ok_or(LevenshteinError::StateLengthOverflow)?;
        let mut distances = try_state_row(state_len)?;
        let dead = u16::from(self.max_edits) + 1;
        distances.extend(
            (0..state_len).map(|distance| {
                u16::try_from(distance).map_or(dead, |distance| distance.min(dead))
            }),
        );
        Ok(LevenshteinState {
            owner: self,
            distances,
            maximum_relevant_distance: dead,
        })
    }

    /// Advances `state` through one candidate byte.
    pub fn try_advance<'automaton>(
        &'automaton self,
        state: &LevenshteinState<'automaton>,
        candidate: u8,
    ) -> Result<LevenshteinState<'automaton>, LevenshteinError> {
        let expected = self.validate_state(state)?;

        let dead = u16::from(self.max_edits) + 1;
        let mut next = try_state_row(expected)?;
        advance_row(&self.pattern, &state.distances, &mut next, candidate, dead);

        Ok(LevenshteinState {
            owner: self,
            distances: next,
            maximum_relevant_distance: dead,
        })
    }

    /// Advances through a compressed ART prefix or complete byte key.
    pub fn try_advance_bytes<'automaton>(
        &'automaton self,
        state: &LevenshteinState<'automaton>,
        bytes: &[u8],
    ) -> Result<LevenshteinState<'automaton>, LevenshteinError> {
        let expected = self.validate_state(state)?;
        let mut current = try_state_row(expected)?;
        current.extend_from_slice(&state.distances);
        let mut next = try_state_row(expected)?;
        let dead = u16::from(self.max_edits) + 1;

        for &byte in bytes {
            advance_row(&self.pattern, &current, &mut next, byte, dead);
            core::mem::swap(&mut current, &mut next);
            next.clear();
            if !row_can_match_descendant(&current, dead) {
                break;
            }
        }
        Ok(LevenshteinState {
            owner: self,
            distances: current,
            maximum_relevant_distance: dead,
        })
    }

    /// Evaluates one complete candidate key.
    pub fn is_match(&self, state: &LevenshteinState<'_>) -> Result<bool, LevenshteinError> {
        self.validate_state(state)?;
        Ok(state
            .distances
            .last()
            .is_some_and(|distance| *distance <= u16::from(self.max_edits)))
    }

    fn validate_state(&self, state: &LevenshteinState<'_>) -> Result<usize, LevenshteinError> {
        if !core::ptr::eq(self, state.owner) {
            return Err(LevenshteinError::StateOwnerMismatch);
        }
        let expected = self
            .pattern
            .len()
            .checked_add(1)
            .ok_or(LevenshteinError::StateLengthOverflow)?;
        if state.distances.len() != expected {
            return Err(LevenshteinError::StateWidthMismatch {
                expected,
                actual: state.distances.len(),
            });
        }
        Ok(expected)
    }
}

fn try_state_row(state_len: usize) -> Result<Vec<u16>, LevenshteinError> {
    let mut row = Vec::new();
    row.try_reserve_exact(state_len)
        .map_err(|_| LevenshteinError::AllocationFailed {
            target: LevenshteinAllocation::State,
            requested: state_len,
        })?;
    Ok(row)
}

fn advance_row(pattern: &[u8], current: &[u16], next: &mut Vec<u16>, candidate: u8, dead: u16) {
    debug_assert!(next.is_empty());
    debug_assert!(next.capacity() >= current.len());
    next.push(current[0].saturating_add(1).min(dead));

    for (pattern_index, &pattern_byte) in pattern.iter().enumerate() {
        let state_index = pattern_index + 1;
        let deletion = next[state_index - 1].saturating_add(1);
        let insertion = current[state_index].saturating_add(1);
        let substitution =
            current[state_index - 1].saturating_add(u16::from(pattern_byte != candidate));
        next.push(deletion.min(insertion).min(substitution).min(dead));
    }
}

fn row_can_match_descendant(row: &[u16], dead: u16) -> bool {
    row.iter()
        .copied()
        .min()
        .is_some_and(|distance| distance < dead)
}

/// One immutable automaton state for an ART path.
#[derive(Debug, Eq, PartialEq)]
pub struct LevenshteinState<'automaton> {
    owner: &'automaton LevenshteinAutomaton,
    distances: Vec<u16>,
    maximum_relevant_distance: u16,
}

impl LevenshteinState<'_> {
    /// Returns whether extending the current key can still reach a match.
    #[must_use]
    pub fn can_match_descendant(&self) -> bool {
        row_can_match_descendant(&self.distances, self.maximum_relevant_distance)
    }

    /// Returns the current complete-key edit distance when it is relevant.
    ///
    /// Values beyond the configured maximum are intentionally collapsed to
    /// `max_edits + 1`, because they are equivalent for subtree pruning.
    #[must_use]
    pub fn terminal_distance(&self) -> Option<u16> {
        self.distances.last().copied()
    }

    /// Number of cells retained by this state.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.distances.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(pattern: &[u8], candidate: &[u8], max_edits: u8) -> bool {
        let automaton =
            LevenshteinAutomaton::try_new(pattern, max_edits, 64).expect("bounded test pattern");
        let initial = automaton.initial_state().expect("bounded state");
        let final_state = automaton
            .try_advance_bytes(&initial, candidate)
            .expect("compatible state");
        automaton.is_match(&final_state).expect("compatible state")
    }

    fn naive_distance(left: &[u8], right: &[u8]) -> usize {
        let mut previous: Vec<usize> = (0..=right.len()).collect();
        let mut current = vec![0; right.len() + 1];
        for (left_index, &left_byte) in left.iter().enumerate() {
            current[0] = left_index + 1;
            for (right_index, &right_byte) in right.iter().enumerate() {
                current[right_index + 1] = (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(previous[right_index] + usize::from(left_byte != right_byte));
            }
            core::mem::swap(&mut previous, &mut current);
        }
        previous[right.len()]
    }

    #[test]
    fn known_edit_distance_examples_match() {
        for (pattern, candidate, distance) in [
            (&b""[..], &b""[..], 0),
            (b"cat", b"cat", 0),
            (b"cat", b"cut", 1),
            (b"cat", b"cats", 1),
            (b"cats", b"cat", 1),
            (b"book", b"back", 2),
            (b"kitten", b"sitting", 3),
        ] {
            for maximum in 0..=MAX_EDIT_DISTANCE {
                assert_eq!(
                    evaluate(pattern, candidate, maximum),
                    distance <= usize::from(maximum),
                    "{pattern:?} vs {candidate:?} at {maximum}"
                );
            }
        }
    }

    #[test]
    fn exhaustive_small_alphabet_matches_naive_oracle() {
        fn words(max_len: usize) -> Vec<Vec<u8>> {
            let mut output = vec![Vec::new()];
            for len in 1..=max_len {
                let count = 1_usize << len;
                for mask in 0..count {
                    let mut word = Vec::with_capacity(len);
                    for bit in 0..len {
                        word.push(if (mask >> bit) & 1 == 0 { b'a' } else { b'b' });
                    }
                    output.push(word);
                }
            }
            output
        }

        let corpus = words(4);
        for pattern in &corpus {
            for candidate in &corpus {
                let expected = naive_distance(pattern, candidate);
                for maximum in 0..=MAX_EDIT_DISTANCE {
                    assert_eq!(
                        evaluate(pattern, candidate, maximum),
                        expected <= usize::from(maximum),
                        "{pattern:?} vs {candidate:?} at {maximum}"
                    );
                }
            }
        }
    }

    #[test]
    fn compressed_prefix_advance_equals_bytewise_advance() {
        let automaton =
            LevenshteinAutomaton::try_new(b"frankengraph", 2, 32).expect("bounded pattern");
        let initial = automaton.initial_state().expect("bounded state");
        let whole = automaton
            .try_advance_bytes(&initial, b"frankengraf")
            .expect("compatible state");
        let mut bytewise = initial;
        for &byte in b"frankengraf" {
            bytewise = automaton
                .try_advance(&bytewise, byte)
                .expect("compatible state");
        }
        assert_eq!(whole, bytewise);
        assert!(automaton.is_match(&whole).expect("compatible state"));
    }

    #[test]
    fn dead_state_prunes_descendants() {
        let automaton = LevenshteinAutomaton::try_new(b"abc", 1, 3).expect("bounded pattern");
        let initial = automaton.initial_state().expect("bounded state");
        let state = automaton
            .try_advance_bytes(&initial, b"zzzzz")
            .expect("compatible state");
        assert!(!state.can_match_descendant());
        assert!(!automaton.is_match(&state).expect("compatible state"));
    }

    #[test]
    fn construction_limits_and_incompatible_states_fail_typed() {
        assert_eq!(
            LevenshteinAutomaton::try_new(b"abc", 3, 3),
            Err(LevenshteinError::UnsupportedDistance {
                requested: 3,
                maximum: 2,
            })
        );
        assert_eq!(
            LevenshteinAutomaton::try_new(b"abc", 2, 2),
            Err(LevenshteinError::PatternLimitExceeded {
                actual: 3,
                limit: 2,
            })
        );

        let automaton = LevenshteinAutomaton::try_new(b"abc", 1, 3).expect("bounded pattern");
        let other = LevenshteinAutomaton::try_new(b"xyz", 1, 3).expect("bounded pattern");
        let wrong = other.initial_state().expect("bounded state");
        assert_eq!(
            automaton.try_advance(&wrong, b'a'),
            Err(LevenshteinError::StateOwnerMismatch)
        );
    }
}
