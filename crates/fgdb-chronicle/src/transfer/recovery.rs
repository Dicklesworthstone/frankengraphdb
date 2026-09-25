//! Retained per-block reconstruction owned exclusively by one BondedPull.
//!
//! The pull owns an append-only record array and its authenticated coordinate
//! index. Neither can be edited through its public API. A cache entry is valid
//! only for that block's exact accepted-record count. New equations invalidate
//! only their own block; every original input remains available for validation.

use super::{BondedPull, PullError, VerifiedObject};
use crate::identity::CryptoVerificationSink;
use crate::symbolize::blocks::recover_indexed_block;
use crate::symbolize::{SymbolizeError, report_recovery, verify_recovered_protected};

#[derive(Clone, Copy, Default)]
struct BlockProgress {
    attempted: usize,
    decoded: bool,
}

struct Round {
    counts: Vec<usize>,
    next: usize,
}

pub(super) struct Recovery {
    progress: Vec<BlockProgress>,
    // One protected-object-sized buffer, never plaintext and never exported.
    // Allocated on the first round, retained across rank failures/cancellation.
    protected: Vec<u8>,
    round: Option<Round>,
    #[cfg(test)]
    pub(super) passes: Vec<u32>,
}

// One step's result, returned and matched at once, never stored in bulk:
// boxing the verified object would add an allocation per completed recovery.
#[allow(clippy::large_enum_variant)]
pub(super) enum Advance {
    Progress,
    AwaitingSymbols,
    Complete(VerifiedObject),
}

enum BlockStep {
    Progress,
    Complete,
    Deficient,
}

impl Recovery {
    pub(super) fn new(blocks: usize) -> Result<Self, PullError> {
        let mut progress = Vec::new();
        progress
            .try_reserve_exact(blocks)
            .map_err(|_| PullError::AllocationFailed)?;
        progress.resize(blocks, BlockProgress::default());
        Ok(Self {
            progress,
            protected: Vec::new(),
            round: None,
            #[cfg(test)]
            passes: vec![0; blocks],
        })
    }

    pub(super) fn target(&self, block: usize, sources: usize, received: usize) -> Option<usize> {
        let progress = self.progress.get(block)?;
        if progress.decoded {
            // An unchanged decoded block needs nothing. A late new equation
            // must be checked, but needs no speculative successor before that.
            Some(received.max(sources))
        } else if progress.attempted == 0 {
            Some(sources)
        } else {
            progress.attempted.checked_add(1)
        }
    }

    pub(super) fn cached_blocks(&self, counts: &[usize]) -> usize {
        self.progress
            .iter()
            .zip(counts)
            .filter(|(progress, count)| progress.decoded && progress.attempted == **count)
            .count()
    }

    fn ready(&self, sources: &[usize], counts: &[usize]) -> bool {
        let mut changed = false;
        for ((progress, sources), count) in self.progress.iter().zip(sources).zip(counts) {
            if progress.decoded && progress.attempted == *count {
                continue;
            }
            if *count < *sources || *count <= progress.attempted {
                return false;
            }
            changed = true;
        }
        changed
    }

    /// Admission of a new equation abandons only unexecuted work in a round.
    /// Already reconstructed blocks remain cached at their exact old counts;
    /// the consumed object-round budget is never refunded.
    pub(super) fn input_changed(&mut self) {
        self.round = None;
    }

    fn begin(&mut self, counts: &[usize], bytes: usize) -> Result<(), SymbolizeError> {
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(counts.len())
            .map_err(|_| SymbolizeError::AllocationFailed)?;
        snapshot.extend_from_slice(counts);
        if self.protected.is_empty() {
            self.protected
                .try_reserve_exact(bytes)
                .map_err(|_| SymbolizeError::AllocationFailed)?;
            self.protected.resize(bytes, 0);
        }
        self.round = Some(Round {
            counts: snapshot,
            next: 0,
        });
        Ok(())
    }

    fn advance(
        &mut self,
        encoding: &crate::identity::EncodedObject,
        records: &[Vec<u8>],
        coordinates: &std::collections::BTreeMap<(u32, u32), usize>,
        dek: &[u8; 32],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<BlockStep, SymbolizeError> {
        let round = self
            .round
            .as_mut()
            .ok_or(SymbolizeError::InvalidParameters)?;
        while round.next < self.progress.len() {
            let block = round.next;
            round.next += 1;
            let count = round.counts[block];
            let progress = &mut self.progress[block];
            if progress.decoded && progress.attempted == count {
                continue;
            }
            // Publish the attempt before invoking user observation/native code.
            // The enclosing pull is fenced against unwind until this returns.
            *progress = BlockProgress {
                attempted: count,
                decoded: false,
            };
            #[cfg(test)]
            {
                self.passes[block] += 1;
            }
            match recover_indexed_block(
                encoding,
                block as u32,
                records,
                coordinates,
                count,
                &mut self.protected,
                dek,
                verification,
            ) {
                Ok(()) => progress.decoded = true,
                Err(SymbolizeError::InsufficientSymbols) => {}
                Err(error) => return Err(error),
            }
            // Do not stop at the first rank failure: retain successes in later
            // blocks as well, and discover every real deficit in this round.
            return Ok(BlockStep::Progress);
        }
        self.round = None;
        Ok(if self.progress.iter().all(|progress| progress.decoded) {
            BlockStep::Complete
        } else {
            BlockStep::Deficient
        })
    }
}

impl BondedPull<'_> {
    pub(super) fn advance_recovery(
        &mut self,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Advance, PullError> {
        self.open()?;
        let starting = self.recovery.round.is_none();
        if starting {
            if !self
                .recovery
                .ready(&self.block_sources, &self.block_received)
            {
                return Ok(Advance::AwaitingSymbols);
            }
            if self.decode_attempts >= self.limits.max_decode_attempts {
                return Err(PullError::DecodeBudget);
            }
            self.decode_attempts += 1;
        }
        // Panic/error while allocating, authenticating, decoding, opening
        // the object or recording evidence cannot reactivate partial state.
        self.closed = true;
        let begun = if starting {
            self.recovery
                .begin(&self.block_received, self.target.protected_len)
        } else {
            Ok(())
        };
        let result = match begun.and_then(|()| {
            self.recovery.advance(
                self.encoding,
                &self.records,
                &self.seen,
                self.dek,
                verification,
            )
        }) {
            Ok(BlockStep::Progress) => {
                self.closed = false;
                return Ok(Advance::Progress);
            }
            Ok(BlockStep::Complete) => verify_recovered_protected(
                self.encoding,
                &self.recovery.protected,
                self.target,
                self.dek,
                verification,
            ),
            Ok(BlockStep::Deficient) => Err(SymbolizeError::InsufficientSymbols),
            Err(error) => Err(error),
        };
        match report_recovery(self.encoding, self.target, result, verification) {
            Ok(plaintext) => {
                self.pending.clear();
                // The returned plaintext now owns the complete verified result;
                // do not retain a second protected object in a closed pull.
                self.recovery.protected = Vec::new();
                Ok(Advance::Complete(VerifiedObject {
                    namespace: self.target.namespace,
                    encoding: self.encoding.clone(),
                    plaintext,
                }))
            }
            Err(SymbolizeError::InsufficientSymbols) => {
                self.closed = false;
                Ok(Advance::AwaitingSymbols)
            }
            Err(error) => Err(PullError::Recovery(error)),
        }
    }
}
