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
pub(super) struct Tape {
    pub(super) runs: Vec<(Token, u32)>,
    polls: u32,
}

impl Tape {
    pub(super) fn poll(&mut self) {
        if self.polls == u32::MAX {
            self.emit(None);
        }
        self.polls += 1;
    }

    pub(super) fn emit(&mut self, step: Option<Step>) {
        let token = Token {
            polls: std::mem::take(&mut self.polls),
            step,
        };
        match self.runs.last_mut() {
            Some((last, run)) if *last == token && *run < u32::MAX => *run += 1,
            _ => self.runs.push((token, 1)),
        }
    }

    pub(super) fn finish(mut self) -> Self {
        if self.polls > 0 {
            self.emit(None);
        }
        self.runs.shrink_to_fit();
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
