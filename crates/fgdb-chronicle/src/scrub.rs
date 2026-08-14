//! Scrub and verify: durability evidence, not durability assertions.
//!
//! `fgdb scrub` samples symbols, verifies them, and re-encodes losses;
//! `fgdb doctor` verifies the chain and reports. Both need an answer richer
//! than a boolean, because the interesting states are in between: *intact*,
//! *degraded but recoverable*, and *lost*. A system that reports only
//! pass/fail cannot tell an operator that bit rot is eating a placement while
//! the object still reads fine — which is precisely the window in which
//! re-encoding is cheap.
//!
//! So a scrub returns a [`ScrubReport`] carrying: how many symbols
//! authenticated, how many were rejected (each rejection IS a detected
//! corruption, located to one symbol by its own MAC), whether the object
//! still decodes, whether its identity recomputed, and — when a decode was
//! attempted — the content hash of asupersync's `DecodeProof`, so the
//! evidence is attestable rather than anecdotal.

use crate::identity::{CryptoVerificationSink, EncodedObject};
use crate::symbol::SymbolRecord;
use crate::symbolize::{RecoveryTarget, SymbolizeError, decode_object};
use asupersync::raptorq::decoder::{InactivationDecoder, ReceivedSymbol};
use asupersync::raptorq::proof::ProofOutcome;
use asupersync::types::ObjectId as RaptorqObjectId;
use std::collections::BTreeMap;

/// What a scrub found. Ordered from healthy to lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubVerdict {
    /// Every sampled symbol authenticated and the object decodes to its
    /// recorded identity. Nothing to do.
    Intact,
    /// Some symbols failed authentication — corruption is present and
    /// located — but enough survive that the object still decodes to its
    /// recorded identity. THIS IS THE MAINTENANCE WINDOW: re-encode the lost
    /// symbols now, while the object is still recoverable.
    Degraded {
        /// Symbols that failed their own MAC, i.e. detected corruption.
        corrupt_symbols: usize,
        /// Repair headroom left: authentic symbols beyond the source count.
        surviving_overhead: usize,
    },
    /// The object no longer decodes to its recorded identity from the symbols
    /// supplied. Fail-closed: no bytes are returned, and the reason is typed
    /// so escalation (replica repair, rebuild-from-suffix, backup) can be
    /// chosen rather than guessed.
    Lost { reason: LostReason },
}

/// Why an object could not be recovered. Each maps to a different escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostReason {
    /// Too few authentic symbols survived — escalate to replica/backup repair
    /// or rebuild-from-suffix for a Chronicle-reconstructible object.
    InsufficientSymbols,
    /// Symbols decoded, but the reassembled ciphertext did not authenticate:
    /// the surviving bytes are internally consistent yet not this object's.
    AuthenticationFailed,
    /// Bytes recovered and opened, but the recomputed `ObjectId` is not the
    /// one recorded. Content addressing says these are not that object.
    IdentityMismatch,
    /// Two independently authenticated records claim the same source-block/ESI
    /// coordinate with different bytes. Choosing either would make recovery
    /// depend on input order, so scrub fails closed instead.
    ConflictingSymbols,
    /// The parameters or symbol framing were not usable at all.
    Unusable,
}

/// The evidence a scrub produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubReport {
    pub verdict: ScrubVerdict,
    /// Symbols presented to the scrub.
    pub symbols_presented: usize,
    /// Symbols that passed their own MAC and encoding binding.
    pub symbols_authentic: usize,
    /// Distinct authenticated source-block/ESI coordinates. Exact duplicate
    /// records authenticate, but they provide no additional repair equation.
    pub symbols_distinct_authentic: usize,
    /// Source-symbol count of this encoding (K).
    pub source_symbols: usize,
    /// Content hash of the RaptorQ decode proof, when a decode ran. This is
    /// asupersync's SHA-256 proof attestation — the artifact that makes a
    /// recovery claim checkable by someone who did not run it.
    pub decode_proof_hash: Option<[u8; 32]>,
}

impl ScrubReport {
    /// Whether an operator should act. `Intact` needs nothing; both other
    /// verdicts are actionable, one cheaply and one urgently.
    pub fn needs_maintenance(&self) -> bool {
        !matches!(self.verdict, ScrubVerdict::Intact)
    }

    /// Symbols that failed authentication: detected, located corruption.
    pub fn corrupt_symbols(&self) -> usize {
        self.symbols_presented - self.symbols_authentic
    }

    /// Authenticated retransmissions that repeated an already-present ESI.
    pub fn duplicate_symbols(&self) -> usize {
        self.symbols_authentic - self.symbols_distinct_authentic
    }
}

/// Scrub one object's symbols and produce evidence.
///
/// Symbol authentication happens first and per symbol, so corruption is
/// *located* rather than merely detected: a failing MAC names exactly which
/// symbol rotted, and that symbol is then treated as an erasure — which is
/// the whole reason symbols carry individual MACs instead of the object
/// carrying one checksum.
pub fn scrub_object(
    encoding: &EncodedObject,
    serialized_symbols: &[Vec<u8>],
    target: RecoveryTarget<'_>,
    dek: &[u8; 32],
    verification: &mut dyn CryptoVerificationSink,
) -> ScrubReport {
    let symbol_size = usize::from(encoding.descriptor().symbol_size);
    let source_symbols = if symbol_size == 0 {
        0
    } else {
        target.protected_len.div_ceil(symbol_size)
    };

    // Pass 1: authenticate every symbol independently. A rejected symbol is a
    // located corruption; it simply does not enter the decode.
    let mut authentic_by_coordinate = BTreeMap::new();
    let mut symbols_authentic = 0usize;
    let mut conflicting_symbols = false;
    for bytes in serialized_symbols {
        if let Ok(record) = SymbolRecord::verify(bytes, encoding, dek, verification) {
            symbols_authentic += 1;
            let coordinate = (record.source_block, record.esi);
            match authentic_by_coordinate.entry(coordinate) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(bytes.clone());
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    if entry.get() != bytes {
                        conflicting_symbols = true;
                    }
                }
            }
        }
    }
    let symbols_distinct_authentic = authentic_by_coordinate.len();
    let authentic: Vec<Vec<u8>> = authentic_by_coordinate.into_values().collect();

    if symbol_size == 0 || source_symbols == 0 {
        return ScrubReport {
            verdict: ScrubVerdict::Lost {
                reason: LostReason::Unusable,
            },
            symbols_presented: serialized_symbols.len(),
            symbols_authentic,
            symbols_distinct_authentic,
            source_symbols,
            decode_proof_hash: None,
        };
    }

    if conflicting_symbols {
        return ScrubReport {
            verdict: ScrubVerdict::Lost {
                reason: LostReason::ConflictingSymbols,
            },
            symbols_presented: serialized_symbols.len(),
            symbols_authentic,
            symbols_distinct_authentic,
            source_symbols,
            decode_proof_hash: None,
        };
    }

    // Pass 2: attempt recovery from the authentic symbols only, and capture
    // the decode proof as attestable evidence.
    let decode_proof_hash = decode_proof_attestation(
        encoding,
        &authentic,
        source_symbols,
        symbol_size,
        dek,
        verification,
    );
    let recovery = decode_object(encoding, &authentic, target, dek, verification);

    let verdict = match recovery {
        Ok(_) => {
            let corrupt = serialized_symbols.len() - symbols_authentic;
            if corrupt == 0 {
                ScrubVerdict::Intact
            } else {
                ScrubVerdict::Degraded {
                    corrupt_symbols: corrupt,
                    surviving_overhead: symbols_distinct_authentic.saturating_sub(source_symbols),
                }
            }
        }
        Err(SymbolizeError::InsufficientSymbols) => ScrubVerdict::Lost {
            reason: LostReason::InsufficientSymbols,
        },
        Err(SymbolizeError::AuthenticationFailed) => ScrubVerdict::Lost {
            reason: LostReason::AuthenticationFailed,
        },
        Err(SymbolizeError::CiphertextIdentityMismatch) => ScrubVerdict::Lost {
            reason: LostReason::IdentityMismatch,
        },
        Err(SymbolizeError::IdentityMismatch) => ScrubVerdict::Lost {
            reason: LostReason::IdentityMismatch,
        },
        Err(_) => ScrubVerdict::Lost {
            reason: LostReason::Unusable,
        },
    };

    ScrubReport {
        verdict,
        symbols_presented: serialized_symbols.len(),
        symbols_authentic,
        symbols_distinct_authentic,
        source_symbols,
        decode_proof_hash,
    }
}

/// Run the decode once more through asupersync's proof-carrying path purely to
/// obtain the attestation hash. The proof records the peeling and elimination
/// traces and its own outcome, so the hash commits to *how* the decode went,
/// not merely that it did.
fn decode_proof_attestation(
    encoding: &EncodedObject,
    authentic_symbols: &[Vec<u8>],
    source_symbols: usize,
    symbol_size: usize,
    dek: &[u8; 32],
    verification: &mut dyn CryptoVerificationSink,
) -> Option<[u8; 32]> {
    // `source_symbols` derives from the descriptor's transfer_length, which
    // is authenticated only by the UNKEYED EncodingId — attacker-rewritable
    // past the systematic-table bound, where the infallible constructor
    // panics. The attestation must not turn that into a process panic ahead
    // of the hardened decode path (fgdb-raptorq-decoder-boundary-panic-hpjb's
    // exact sibling).
    let decoder =
        InactivationDecoder::try_new(source_symbols, symbol_size, code_seed(encoding)).ok()?;
    let mut received = decoder.constraint_symbols();
    for bytes in authentic_symbols {
        let record = SymbolRecord::verify(bytes, encoding, dek, verification).ok()?;
        if (record.esi as usize) < source_symbols {
            received.push(ReceivedSymbol::source(record.esi, record.payload));
        } else {
            let (columns, coefficients) = decoder.repair_equation(record.esi).ok()?;
            received.push(ReceivedSymbol::repair(
                record.esi,
                columns,
                coefficients,
                record.payload,
            ));
        }
    }

    // The proof's object id is the RaptorQ transport identity, not Chronicle's
    // keyed ObjectId; use the encoding identity's high bits so the proof is
    // attributable to this exact encoding.
    let id = encoding.encoding_id();
    let high = u64::from_be_bytes([
        id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7],
    ]);
    let low = u64::from_be_bytes([
        id.0[8], id.0[9], id.0[10], id.0[11], id.0[12], id.0[13], id.0[14], id.0[15],
    ]);

    // Both arms carry a proof: a FAILED decode is evidence too, and its
    // outcome names the failure reason.
    let proof = match decoder.decode_with_proof(&received, RaptorqObjectId::new(high, low), 0) {
        Ok(with_proof) => with_proof.proof,
        Err((_, proof)) => proof,
    };
    debug_assert!(
        matches!(
            proof.outcome,
            ProofOutcome::Success { .. } | ProofOutcome::Failure { .. }
        ),
        "a decode proof always records an outcome"
    );
    Some(*proof.content_hash().as_bytes())
}

fn code_seed(encoding: &EncodedObject) -> u64 {
    let id = encoding.encoding_id();
    u64::from_be_bytes([
        id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7],
    ])
}
