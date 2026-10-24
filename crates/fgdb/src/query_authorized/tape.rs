//! The live calls of an authorized source build, recorded for exact replay.
//!
//! A reuse of anything an authorized read built (a Prism projection,
//! fgdb-0tbrz; a full scan's admitted records, fgdb-61p1n) must spend what
//! the build spent: the same charge-free source polls, signed and native
//! charges and checkpoints, in the same order, so every refusal point and
//! cancellation ordinal is that of a fresh build. A build records them here;
//! a reuse replays them through the same closures the build called.

use std::mem::size_of;

/// A live call of a build, other than a charge-free source poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// An admitted vertex, with the number of its labels the scope shows.
    Vertex(u32),
    /// An admitted edge.
    Edge,
    /// A builder checkpoint.
    Checkpoint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_tapes_preserve_all_events_or_explicitly_disable_reuse() {
        let events = [
            (2, Some(Step::Vertex(1))),
            (2, Some(Step::Vertex(1))),
            (7, Some(Step::Edge)),
            (0, Some(Step::Checkpoint)),
            (0, Some(Step::Checkpoint)),
            (3, None),
        ];
        let record = |tape: &mut Tape| {
            for (polls, step) in events {
                for _ in 0..polls {
                    tape.poll();
                }
                if let Some(step) = step {
                    tape.emit(Some(step));
                }
            }
        };
        let mut oracle = Tape::default();
        record(&mut oracle);
        let oracle = oracle.finish();
        let expected: Vec<_> = oracle.runs().collect();
        for max_runs in 0..=8 {
            let bytes = size_of::<Tape>() + max_runs * size_of::<(Token, u32)>();
            let mut actual = Tape::bounded(bytes);
            record(&mut actual);
            let actual = actual.finish();
            assert!(actual.bytes() <= bytes);
            if max_runs >= expected.len() {
                assert!(actual.is_complete());
                assert_eq!(actual.runs().collect::<Vec<_>>(), expected);
            } else {
                assert!(!actual.is_complete());
            }
        }
        let mut refused = Tape::bounded(0);
        record(&mut refused);
        assert!(!refused.finish().is_complete());
    }

    #[test]
    fn a_full_bounded_tape_still_coalesces_but_never_reuses_a_truncated_prefix() {
        let bytes = size_of::<Tape>() + size_of::<(Token, u32)>();
        let mut tape = Tape::bounded(bytes);
        for _ in 0..1_000 {
            tape.poll();
            tape.emit(Some(Step::Edge));
        }
        assert!(tape.is_complete());
        assert_eq!(
            tape.runs().collect::<Vec<_>>(),
            [(1, Some(Step::Edge), 1_000)]
        );
        tape.emit(Some(Step::Checkpoint));
        assert!(!tape.is_complete());
        let prefix: Vec<_> = tape.runs().collect();
        for _ in 0..100 {
            tape.poll();
            tape.emit(Some(Step::Edge));
        }
        let tape = tape.finish();
        assert!(!tape.is_complete());
        assert_eq!(tape.runs().collect::<Vec<_>>(), prefix);
        assert!(tape.bytes() <= bytes);
    }
}

/// `polls` charge-free source polls, then `step` (none: polls only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Token {
    polls: u32,
    step: Option<Step>,
}

/// Every live call a successful build made, in order and run-length coded.
/// Over a single-version snapshot it is a handful of runs: one per poll and
/// admission pattern, not one per record.
#[derive(Debug, Default)]
pub(crate) struct Tape {
    pub(super) runs: Vec<(Token, u32)>,
    polls: u32,
    limit: Option<usize>,
    truncated: bool,
}

impl Tape {
    /// Optional cache metadata must not grow without bound while a query is
    /// building its result. If recording cannot fit, leave the query's live
    /// work alone and disable reuse of this partial tape.
    pub(super) fn bounded(max_bytes: usize) -> Self {
        Self {
            limit: Some(max_bytes),
            truncated: max_bytes < size_of::<Self>(),
            ..Self::default()
        }
    }

    pub(super) fn is_complete(&self) -> bool {
        !self.truncated
    }

    pub(super) fn poll(&mut self) {
        if self.truncated {
            return;
        }
        if self.polls == u32::MAX {
            self.emit(None);
        }
        self.polls += 1;
    }

    pub(super) fn emit(&mut self, step: Option<Step>) {
        if self.truncated {
            return;
        }
        let token = Token {
            polls: std::mem::take(&mut self.polls),
            step,
        };
        match self.runs.last_mut() {
            Some((last, run)) if *last == token && *run < u32::MAX => *run += 1,
            _ => {
                if let Some(limit) = self.limit
                    && self.runs.len() == self.runs.capacity()
                {
                    let max_runs =
                        limit.saturating_sub(size_of::<Self>()) / size_of::<(Token, u32)>();
                    let capacity = self.runs.capacity().saturating_mul(2).max(4).min(max_runs);
                    if capacity <= self.runs.len()
                        || self
                            .runs
                            .try_reserve_exact(capacity - self.runs.len())
                            .is_err()
                        || self.runs.capacity() > max_runs
                    {
                        if self.runs.capacity() > max_runs {
                            // An allocator may grant more than requested.
                            // Do not retain an unusable, over-budget vector.
                            self.runs = Vec::new();
                        }
                        self.truncated = true;
                        return;
                    }
                }
                self.runs.push((token, 1));
            }
        }
    }

    pub(super) fn finish(mut self) -> Self {
        if self.polls > 0 {
            self.emit(None);
        }
        if !self.truncated {
            self.runs.shrink_to_fit();
        }
        self
    }

    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>() + self.runs.capacity() * size_of::<(Token, u32)>()
    }

    /// The runs in order: `count` repetitions of `polls` charge-free polls
    /// then `step` (none: polls only), exactly what `replay` walks.
    pub(super) fn runs(&self) -> impl Iterator<Item = (u32, Option<Step>, u32)> + '_ {
        self.runs
            .iter()
            .map(|&(token, count)| (token.polls, token.step, count))
    }

    pub(super) fn replay<E>(
        &self,
        mut poll: impl FnMut() -> Result<(), E>,
        mut spend: impl FnMut(Step) -> Result<(), E>,
    ) -> Result<(), E> {
        for &(token, run) in &self.runs {
            for _ in 0..run {
                for _ in 0..token.polls {
                    poll()?;
                }
                if let Some(step) = token.step {
                    spend(step)?;
                }
            }
        }
        Ok(())
    }
}
