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
    use std::cell::{Cell, RefCell};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Live {
        Poll,
        Spend(Step),
    }

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
        let expected = [
            (2, Some(Step::Vertex(1)), 2),
            (7, Some(Step::Edge), 1),
            (0, Some(Step::Checkpoint), 2),
            (3, None, 1),
        ];
        // The completed prefix is 3 + 1 + 2 bytes; the final run lives in
        // the fixed-size tail. Check every allowance around that exact bound.
        for payload_bytes in 0..=8 {
            let bytes = size_of::<Tape>() + payload_bytes;
            let mut actual = Tape::bounded(bytes);
            record(&mut actual);
            let actual = actual.finish();
            assert!(actual.bytes() <= bytes);
            if payload_bytes >= 6 {
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
        // No heap allowance: an arbitrary final repetition count still fits.
        let bytes = size_of::<Tape>();
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

    #[test]
    fn packed_runs_match_independent_events_at_every_refusing_callback() {
        let mut tape = Tape::default();
        let mut events = Vec::new();
        let mut expected_runs: Vec<(u32, Option<Step>, u32)> = Vec::new();
        // Several step kinds and small/extended poll counts, with both runs
        // and irregular patterns. The oracle is the direct live event log,
        // independent of the packed representation and its decoder.
        for index in 0..70 {
            let polls = [0, 1, 30, 31, 32, 127, 128][index / 10];
            let step = match index % 10 {
                0..=2 => Some(Step::Vertex((index / 10) as u32)),
                3..=5 => Some(Step::Edge),
                6..=8 => Some(Step::Checkpoint),
                _ => None,
            };
            for _ in 0..polls {
                events.push(Live::Poll);
                tape.poll();
            }
            if let Some(step) = step {
                events.push(Live::Spend(step));
            }
            tape.emit(step);
            match expected_runs.last_mut() {
                Some((old_polls, old_step, count)) if (*old_polls, *old_step) == (polls, step) => {
                    *count += 1;
                }
                _ => expected_runs.push((polls, step, 1)),
            }
        }
        for _ in 0..3 {
            events.push(Live::Poll);
            tape.poll();
        }
        expected_runs.push((3, None, 1));
        let tape = tape.finish();
        assert!(tape.is_complete());
        assert_eq!(tape.runs().collect::<Vec<_>>(), expected_runs);
        for stop in 0..=events.len() {
            let observed = RefCell::new(Vec::new());
            let ordinal = Cell::new(0);
            let visit = |event| {
                observed.borrow_mut().push(event);
                let at = ordinal.replace(ordinal.get() + 1);
                if at == stop { Err(event) } else { Ok(()) }
            };
            let actual = tape.replay(|| visit(Live::Poll), |step| visit(Live::Spend(step)));
            assert_eq!(actual, events.get(stop).copied().map_or(Ok(()), Err));
            assert_eq!(*observed.borrow(), events[..events.len().min(stop + 1)]);
        }
    }

    #[test]
    fn complete_u32_fields_roundtrip_across_every_encoding_width() {
        let values = [
            0,
            1,
            30,
            31,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            268_435_455,
            268_435_456,
            u32::MAX,
        ];
        // Build encoded full runs directly to test values that would require
        // billions of live calls to accumulate. Encoding is the production
        // helper; expected tuples are independent, unencoded u32 values.
        let mut tape = Tape::default();
        let mut expected = Vec::new();
        for polls in values {
            for step in [None, Some(Step::Edge), Some(Step::Checkpoint)]
                .into_iter()
                .chain(values.into_iter().map(|labels| Some(Step::Vertex(labels))))
            {
                for count in values.into_iter().filter(|&count| count > 0) {
                    let mut encoded = [0; 16];
                    let len = encode_run(Token { polls, step }, count, &mut encoded);
                    assert!((1..=16).contains(&len));
                    assert!(tape.append(&encoded[..len]));
                    expected.push((polls, step, count));
                }
            }
        }
        assert_eq!(tape.runs().collect::<Vec<_>>(), expected);
        let mut encoded = [0; 16];
        let len = encode_run(
            Token {
                polls: 128,
                step: Some(Step::Vertex(16_384)),
            },
            128,
            &mut encoded,
        );
        assert_eq!(&encoded[..len], &[253, 128, 1, 128, 128, 1, 128, 1]);
    }

    #[test]
    fn overflowing_poll_and_repetition_counters_split_without_losing_calls() {
        let mut tape = Tape {
            polls: u32::MAX,
            ..Tape::default()
        };
        tape.poll();
        tape.emit(Some(Step::Edge));
        assert_eq!(
            tape.finish().runs().collect::<Vec<_>>(),
            [(u32::MAX, None, 1), (1, Some(Step::Edge), 1)]
        );

        let token = Token {
            polls: 0,
            step: Some(Step::Checkpoint),
        };
        let mut tape = Tape {
            tail: Some((token, u32::MAX - 1)),
            ..Tape::bounded(size_of::<Tape>() + 6)
        };
        tape.emit(token.step);
        assert_eq!(tape.runs().collect::<Vec<_>>(), [(0, token.step, u32::MAX)]);
        assert_eq!(tape.bytes(), size_of::<Tape>());
        tape.emit(token.step);
        let tape = tape.finish();
        assert!(tape.is_complete());
        assert_eq!(
            tape.runs().collect::<Vec<_>>(),
            [(0, token.step, u32::MAX), (0, token.step, 1)]
        );
        assert_eq!(tape.bytes(), size_of::<Tape>() + 6);
    }

    #[test]
    fn irregular_overlay_descent_runs_fit_one_byte_each_without_changing_polls() {
        let records = 65_536usize;
        let budget = size_of::<Tape>() + records;
        let mut tape = Tape::bounded(budget);
        for index in 1..=records {
            let polls = 2 + index.trailing_zeros();
            for _ in 0..polls {
                tape.poll();
            }
            tape.emit(Some(Step::Edge));
            assert!(tape.bytes() <= budget);
        }
        let tape = tape.finish();
        assert!(tape.is_complete());
        assert_eq!(tape.bytes(), size_of::<Tape>() + records - 1);
        assert_eq!(tape.runs().count(), records);
        for (index, run) in tape.runs().enumerate() {
            assert_eq!(run, (2 + (index + 1).trailing_zeros(), Some(Step::Edge), 1));
        }
        // The old tuple representation needed sixteen bytes for EACH run,
        // so this very same allowance could not have retained its full tape.
        assert_eq!(size_of::<(Token, u32)>(), 16);
        assert!(records * size_of::<(Token, u32)>() > budget);
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
/// admission pattern, not one per record. Edges a retained overlay index
/// serves are the exception: their cursor charges its actual tree descent
/// (a0f6c8101), so neighbouring edges rarely share a run. Store those small
/// irregular runs in one byte each instead of one full-width tuple. The last
/// run stays in fixed-size metadata so coalescing never needs more allocation.
#[derive(Debug, Default)]
pub(crate) struct Tape {
    encoded: Vec<u8>,
    tail: Option<(Token, u32)>,
    polls: u32,
    limit: Option<usize>,
    truncated: bool,
}

// Internal, immutable cache metadata, never a durable or untrusted wire format.
// A header holds the step kind (2 bits), polls (5 bits, 31 escapes to a u32
// varint), and a repeated-run flag. Vertex labels and repeated counts are u32
// varints. All u32 values remain exact; a complete record needs at most 16 bytes.
fn encode_run(token: Token, count: u32, output: &mut [u8; 16]) -> usize {
    debug_assert_ne!(count, 0);
    let kind = match token.step {
        None => 0,
        Some(Step::Vertex(_)) => 1,
        Some(Step::Edge) => 2,
        Some(Step::Checkpoint) => 3,
    };
    output[0] = kind | ((token.polls.min(31) as u8) << 2) | (u8::from(count > 1) << 7);
    let mut len = 1;
    if token.polls >= 31 {
        encode_u32(token.polls, output, &mut len);
    }
    if let Some(Step::Vertex(labels)) = token.step {
        encode_u32(labels, output, &mut len);
    }
    if count > 1 {
        encode_u32(count, output, &mut len);
    }
    len
}

fn encode_u32(mut value: u32, output: &mut [u8; 16], len: &mut usize) {
    while value >= 128 {
        output[*len] = (value as u8 & 0x7f) | 0x80;
        *len += 1;
        value >>= 7;
    }
    output[*len] = value as u8;
    *len += 1;
}

fn decode_u32(bytes: &mut &[u8]) -> u32 {
    let mut value = 0;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = bytes.split_first().expect("complete internal tape varint");
        *bytes = rest;
        debug_assert!(shift != 28 || byte <= 15);
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
    }
    unreachable!("internal tape values are u32")
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
            if self.truncated {
                return;
            }
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
        match self.tail.as_mut() {
            Some((last, run)) if *last == token && *run < u32::MAX => *run += 1,
            _ => {
                if let Some((last, count)) = self.tail {
                    let mut bytes = [0; 16];
                    let len = encode_run(last, count, &mut bytes);
                    if !self.append(&bytes[..len]) {
                        self.truncated = true;
                        return;
                    }
                }
                self.tail = Some((token, 1));
            }
        }
    }

    fn append(&mut self, bytes: &[u8]) -> bool {
        let Some(limit) = self.limit else {
            // Full-scan admission uses an unbounded tape and relies on its
            // complete transcript. Preserve its original Vec allocation
            // contract; only bounded optional recording may truncate.
            self.encoded.extend_from_slice(bytes);
            return true;
        };
        let maximum = limit.saturating_sub(size_of::<Self>());
        let Some(required) = self.encoded.len().checked_add(bytes.len()) else {
            return false;
        };
        if required > maximum {
            return false;
        }
        if required > self.encoded.capacity() {
            let capacity = self
                .encoded
                .capacity()
                .saturating_mul(2)
                .max(64)
                .max(required)
                .min(maximum);
            if self
                .encoded
                .try_reserve_exact(capacity - self.encoded.len())
                .is_err()
            {
                return false;
            }
            if self.encoded.capacity() > maximum {
                // Keep no over-budget allocation or incomplete decoded suffix.
                // An empty tape is a prefix too, and cannot authorize reuse.
                self.encoded = Vec::new();
                self.tail = None;
                return false;
            }
        }
        self.encoded.extend_from_slice(bytes);
        true
    }

    pub(super) fn finish(mut self) -> Self {
        if self.polls > 0 {
            self.emit(None);
        }
        if !self.truncated {
            self.encoded.shrink_to_fit();
        }
        self
    }

    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>() + self.encoded.capacity()
    }

    /// The runs in order: `count` repetitions of `polls` charge-free polls
    /// then `step` (none: polls only), exactly what `replay` walks.
    pub(super) fn runs(&self) -> impl Iterator<Item = (u32, Option<Step>, u32)> + '_ {
        let mut bytes = self.encoded.as_slice();
        std::iter::from_fn(move || {
            let (&header, rest) = bytes.split_first()?;
            bytes = rest;
            let short_polls = (header >> 2) & 31;
            let polls = if short_polls == 31 {
                decode_u32(&mut bytes)
            } else {
                u32::from(short_polls)
            };
            let step = match header & 3 {
                0 => None,
                1 => Some(Step::Vertex(decode_u32(&mut bytes))),
                2 => Some(Step::Edge),
                _ => Some(Step::Checkpoint),
            };
            let count = if header & 0x80 != 0 {
                decode_u32(&mut bytes)
            } else {
                1
            };
            Some((polls, step, count))
        })
        .chain(
            self.tail
                .iter()
                .map(|&(token, count)| (token.polls, token.step, count)),
        )
    }

    pub(super) fn replay<E>(
        &self,
        mut poll: impl FnMut() -> Result<(), E>,
        mut spend: impl FnMut(Step) -> Result<(), E>,
    ) -> Result<(), E> {
        for (polls, step, run) in self.runs() {
            for _ in 0..run {
                for _ in 0..polls {
                    poll()?;
                }
                if let Some(step) = step {
                    spend(step)?;
                }
            }
        }
        Ok(())
    }
}
