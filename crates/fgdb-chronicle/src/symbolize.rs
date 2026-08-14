//! RaptorQ symbolization: protected bytes → authenticated symbols → protected
//! bytes, with erasure recovery inside the declared repair budget.
//!
//! The RFC 6330 codec itself is **consumed, not reimplemented** — that is the
//! closed-universe rule working as intended: asupersync owns a fuzz-hardened,
//! conformance-tested RaptorQ, and Chronicle owns the durable framing,
//! authentication, and identity recomputation around it.
//!
//! THE LAW THIS FILE EXISTS TO ENFORCE (FG-INV-09): a recovered object is not
//! "probably right". [`decode_object`] reassembles, opens the AEAD, and
//! **recomputes the keyed `ObjectId` from the recovered plaintext**, returning
//! bytes only when it equals the identity that was asked for. Erasure recovery
//! that silently returns different bytes is the failure mode content
//! addressing exists to make impossible, so it is checked, not assumed.

use crate::identity::{EncodedObject, RecoveredObjectError};
use crate::symbol::{SymbolError, SymbolRecord};
use asupersync::raptorq::decoder::{DecodeError, InactivationDecoder, ReceivedSymbol};
use asupersync::raptorq::systematic::SystematicEncoder;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

/// Why symbolization or recovery failed. Every variant is fail-closed: a
/// caller never receives plaintext from a decode that did not fully verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolizeError {
    /// `symbol_size` was zero, or the object does not fit the declared
    /// source-block model.
    InvalidParameters,
    /// The RFC 6330 encoder rejected these parameters.
    EncoderUnavailable,
    /// Heap storage for source symbols or serialized records was refused.
    AllocationFailed,
    /// Fewer independent symbols survived than the code needs. This is the
    /// honest "beyond the repair budget" outcome.
    InsufficientSymbols,
    /// A symbol failed its own authentication or binding checks.
    Symbol(SymbolError),
    /// The AEAD did not open — the recovered ciphertext was not authentic.
    AuthenticationFailed,
    /// The AEAD opened, but descriptor+ciphertext+tag did not recompute the
    /// durable `CiphertextId` carried by the admitted encoding.
    CiphertextIdentityMismatch,
    /// THE IDENTITY LAW FIRED: bytes were recovered and opened, but their
    /// recomputed `ObjectId` is not the one requested. Content addressing
    /// says these are simply not that object.
    IdentityMismatch,
    /// The RFC 6330 decode itself rejected the received set — structural
    /// corruption (size/column/output violations) or a blown compute budget,
    /// never rank deficiency, which is reported separately as
    /// [`Self::InsufficientSymbols`] from the recovered source length.
    DecodeFailed,
}

impl From<SymbolError> for SymbolizeError {
    fn from(error: SymbolError) -> Self {
        Self::Symbol(error)
    }
}

impl core::fmt::Display for SymbolizeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidParameters => f.write_str("invalid symbolization parameters"),
            Self::EncoderUnavailable => {
                f.write_str("the RaptorQ encoder rejected these parameters")
            }
            Self::AllocationFailed => {
                f.write_str("the RaptorQ encoder could not reserve bounded working storage")
            }
            Self::InsufficientSymbols => {
                f.write_str("too few symbols survived to decode: beyond the repair budget")
            }
            Self::Symbol(error) => write!(f, "symbol rejected: {error}"),
            Self::AuthenticationFailed => f.write_str("recovered ciphertext failed authentication"),
            Self::CiphertextIdentityMismatch => {
                f.write_str("recovered ciphertext does not recompute the declared CiphertextId")
            }
            Self::IdentityMismatch => {
                f.write_str("recovered bytes do not recompute the requested ObjectId")
            }
            Self::DecodeFailed => f.write_str(
                "the RaptorQ decode rejected the received set (structural corruption or compute budget)",
            ),
        }
    }
}

impl core::error::Error for SymbolizeError {}

/// The seed the RFC 6330 tuple generator uses for one encoding. Derived from
/// the `EncodingId` so it is a deterministic function of the encoding — two
/// peers holding the same authenticated descriptor derive the same code, and
/// nothing extra has to travel with the symbols.
fn code_seed(encoding: &EncodedObject) -> u64 {
    let id = encoding.encoding_id();
    u64::from_be_bytes([
        id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7],
    ])
}

/// How many source symbols an object of `protected_len` bytes occupies.
pub fn source_symbol_count(protected_len: usize, symbol_size: u16) -> usize {
    if symbol_size == 0 {
        return 0;
    }
    protected_len.div_ceil(usize::from(symbol_size))
}

/// The erasure budget of a symbol set: how many symbols may be lost and still
/// recover.
///
/// This is exactly the repair-symbol count, because the decoder supplies the
/// RFC's LDPC/HDPC constraint equations itself (they are a property of the
/// code, not transmitted data). MEASURED while building this: omitting those
/// constraint equations makes even a COMPLETE symbol set fail to decode with
/// `InsufficientSymbols`, because the system is then rank-deficient by exactly
/// `L - K`. The constraints are therefore not optional plumbing — they are
/// what makes the repair budget mean what it says.
pub fn erasure_budget(repair_symbols: usize) -> usize {
    repair_symbols
}

const MAX_SOURCE_SYMBOLS_PER_BLOCK: usize = 56_403;

/// Allocation authority for exactly one RFC-valid source block.
///
/// Keeping the constructor private makes the ordering law structural:
/// source-symbol storage cannot be materialized until the zero/empty checks
/// and the RFC systematic-table ceiling have all passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValidatedSourceBlock {
    symbol_size: usize,
    source_symbols: usize,
}

impl ValidatedSourceBlock {
    fn try_new(protected_len: usize, symbol_size: u16) -> Result<Self, SymbolizeError> {
        let symbol_size = usize::from(symbol_size);
        if symbol_size == 0 || protected_len == 0 {
            return Err(SymbolizeError::InvalidParameters);
        }
        let source_symbols = protected_len.div_ceil(symbol_size);
        if source_symbols > MAX_SOURCE_SYMBOLS_PER_BLOCK {
            return Err(SymbolizeError::InvalidParameters);
        }
        Ok(Self {
            symbol_size,
            source_symbols,
        })
    }

    /// Split the protected bytes into K equal source symbols, zero-padding the
    /// final one. `transfer_length` in the encoding descriptor is what tells
    /// the decoder how much of the last symbol is real.
    fn materialize(self, protected: &[u8]) -> Result<Vec<Vec<u8>>, SymbolizeError> {
        debug_assert_eq!(
            protected.len().div_ceil(self.symbol_size),
            self.source_symbols
        );
        let mut source = Vec::new();
        source
            .try_reserve_exact(self.source_symbols)
            .map_err(|_| SymbolizeError::AllocationFailed)?;
        for chunk in protected.chunks(self.symbol_size) {
            let mut symbol = Vec::new();
            symbol
                .try_reserve_exact(self.symbol_size)
                .map_err(|_| SymbolizeError::AllocationFailed)?;
            symbol.resize(self.symbol_size, 0);
            symbol[..chunk.len()].copy_from_slice(chunk);
            source.push(symbol);
        }
        Ok(source)
    }
}

/// Encode one protected object into authenticated symbol records: every source
/// symbol, plus `repair_symbols` repair symbols.
///
/// `repair_symbols` is the object's share of the configured repair overhead
/// (`fgdb.toml`'s `repair_overhead`); the caller prices it, because the plan
/// makes the budget a policy decision per reconstructibility tier, not a
/// constant.
pub fn encode_object(
    encoding: &EncodedObject,
    protected: &[u8],
    object_kind: u16,
    source_block: u32,
    repair_symbols: u32,
    dek: &[u8; 32],
) -> Result<Vec<Vec<u8>>, SymbolizeError> {
    // The encoded stage is the sole authority for durable symbol identity.
    // Keep this caller-facing cross-check while callers still carry an object
    // kind for their own contracts, but never pass it into `SymbolRecord`.
    if object_kind != encoding.cipher_descriptor().object_kind {
        return Err(SymbolizeError::InvalidParameters);
    }
    // asupersync's parameter builder accepts k = 1..=56403 and PANICS outside
    // it (k == 0 is precluded by ValidatedSourceBlock's own checks); the RFC
    // 6330 systematic table itself starts at 4, and asupersync accepts down to
    // 1. The bound lives here instead of in the panic: a large object is a
    // typed refusal, and the check precedes every allocation on this path.
    let source_shape =
        ValidatedSourceBlock::try_new(protected.len(), encoding.descriptor().symbol_size)?;
    let source = source_shape.materialize(protected)?;
    let k = u32::try_from(source_shape.source_symbols)
        .map_err(|_| SymbolizeError::InvalidParameters)?;
    let symbol_size = source_shape.symbol_size;
    let encoder = SystematicEncoder::new(&source, symbol_size, code_seed(encoding))
        .ok_or(SymbolizeError::EncoderUnavailable)?;

    let k_symbol = encoding.symbol_auth_key(dek);
    let record_count = source
        .len()
        .checked_add(
            usize::try_from(repair_symbols).map_err(|_| SymbolizeError::InvalidParameters)?,
        )
        .ok_or(SymbolizeError::InvalidParameters)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(record_count)
        .map_err(|_| SymbolizeError::AllocationFailed)?;

    // Source symbols carry ESI 0..K; repair symbols continue from K, which is
    // the RFC's own numbering, so the decoder needs no side channel.
    for (index, symbol) in source.iter().enumerate() {
        let esi = u32::try_from(index).map_err(|_| SymbolizeError::InvalidParameters)?;
        let record = SymbolRecord::for_encoding(encoding, source_block, esi, 0, symbol.clone());
        records.push(record.serialize(&k_symbol));
    }
    for offset in 0..repair_symbols {
        let esi = k
            .checked_add(offset)
            .ok_or(SymbolizeError::InvalidParameters)?;
        let payload = encoder
            .try_repair_symbol(esi)
            .map_err(|_| SymbolizeError::InvalidParameters)?;
        let record = SymbolRecord::for_encoding(encoding, source_block, esi, 0, payload);
        records.push(record.serialize(&k_symbol));
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::{MAX_SOURCE_SYMBOLS_PER_BLOCK, SymbolizeError, ValidatedSourceBlock};

    #[test]
    fn source_block_ceiling_is_validated_before_materialization_authority_exists() {
        let maximum = ValidatedSourceBlock::try_new(MAX_SOURCE_SYMBOLS_PER_BLOCK, 1)
            .expect("the RFC ceiling is admissible");
        assert_eq!(maximum.source_symbols, MAX_SOURCE_SYMBOLS_PER_BLOCK);
        assert_eq!(
            ValidatedSourceBlock::try_new(MAX_SOURCE_SYMBOLS_PER_BLOCK + 1, 1),
            Err(SymbolizeError::InvalidParameters)
        );
    }
}

/// What a caller is asking recovery to produce: the exact object, named by the
/// identity inputs that let recovery PROVE it produced that object rather than
/// merely some bytes. Bundling them is not cosmetic — it makes it impossible to
/// pass a recovery target that is missing a transcript input, which is how an
/// identity check silently degrades into a length check.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryTarget<'a> {
    /// The database identity key the ObjectId is keyed under.
    pub k_oid: &'a [u8; 32],
    /// The security namespace inside the identity transcript.
    pub namespace: DatabaseSecurityNamespaceId,
    /// The identity the recovered bytes must recompute.
    pub object_id: ObjectId,
    /// The canonical header half of the identity transcript.
    pub canonical_header: &'a [u8],
    /// Length of the protected (sealed) bytes, from the authenticated
    /// encoding descriptor — it fixes K and trims the final symbol's padding.
    pub protected_len: usize,
}

/// Recover a protected object from whatever authenticated symbols survive, and
/// prove the result is the object that was asked for.
///
/// `source_symbol_count` is K — the decoder needs it, and it comes from the
/// authenticated encoding descriptor's transfer length and symbol size rather
/// than from any individual symbol, because a symbol never authorizes itself.
///
/// Returns the recovered *compressed plaintext* (the bytes that went into the
/// AEAD), having already: authenticated every symbol, decoded, opened the
/// AEAD, and recomputed the keyed `ObjectId`.
pub fn decode_object(
    encoding: &EncodedObject,
    serialized_symbols: &[Vec<u8>],
    target: RecoveryTarget<'_>,
    dek: &[u8; 32],
) -> Result<Vec<u8>, SymbolizeError> {
    let RecoveryTarget {
        k_oid,
        namespace,
        object_id: expected_object_id,
        canonical_header,
        protected_len,
    } = target;
    let symbol_size = usize::from(encoding.descriptor().symbol_size);
    if symbol_size == 0 || protected_len == 0 {
        return Err(SymbolizeError::InvalidParameters);
    }
    let k = protected_len.div_ceil(symbol_size);

    // Every symbol is authenticated against the encoding BEFORE it can
    // influence a decode: a forged or foreign symbol must not even enter the
    // linear system, let alone perturb the recovered bytes.
    //
    // `EncodingId` is an unkeyed digest, so descriptor self-consistency does
    // not authenticate `k`. Use the fallible constructor before it can turn an
    // attacker-rewritten transfer length into a process panic.
    let decoder = InactivationDecoder::try_new(k, symbol_size, code_seed(encoding))
        .map_err(|_| SymbolizeError::InvalidParameters)?;

    // The decoder's own LDPC/HDPC constraint equations seed the system. They
    // are derived from the code parameters, never transmitted, so they cost no
    // durable bytes and cannot be forged by supplying symbols.
    let mut received = decoder.constraint_symbols();
    for bytes in serialized_symbols {
        // Authenticate BEFORE the symbol can influence the decode: a forged or
        // foreign symbol must not enter the linear system at all.
        let record = SymbolRecord::verify(bytes, encoding, dek)?;
        if (record.esi as usize) < k {
            received.push(ReceivedSymbol::source(record.esi, record.payload));
        } else {
            let (columns, coefficients) = decoder
                .repair_equation(record.esi)
                .map_err(|_| SymbolizeError::InvalidParameters)?;
            received.push(ReceivedSymbol::repair(
                record.esi,
                columns,
                coefficients,
                record.payload,
            ));
        }
    }

    let decoded = decoder
        // This is the RFC 6330 erasure decoder; no JWT or signature state exists here.
        // ubs:ignore -- exact false match is `InactivationDecoder::decode`, not a JWT decoder.
        .decode(&received)
        .map_err(|error| match error {
            // Rank deficiency — too few independent equations — is the one
            // outcome "beyond the repair budget" honestly names: scrub must
            // route it to repair, not to the corruption path.
            DecodeError::InsufficientSymbols { .. } | DecodeError::SingularMatrix { .. } => {
                SymbolizeError::InsufficientSymbols
            }
            // Everything else is structural (size/arity/column/ESI/output
            // violations) or a budget/rate-limit refusal: the received set is
            // unusable, and calling it "insufficient" would have a scrubber
            // fetch MORE copies of poison.
            _ => SymbolizeError::DecodeFailed,
        })?;
    if decoded.source.len() < k {
        return Err(SymbolizeError::InsufficientSymbols);
    }

    let mut protected = Vec::with_capacity(k * symbol_size);
    for symbol in decoded.source.iter().take(k) {
        protected.extend_from_slice(symbol);
    }
    protected.truncate(protected_len);

    // Layer 1 of the check: the AEAD must open. This already rejects any
    // decode that produced different ciphertext.
    let compressed = encoding
        .open_recovered(&protected, dek)
        .map_err(|error| match error {
            RecoveredObjectError::AuthenticationFailed => SymbolizeError::AuthenticationFailed,
            RecoveredObjectError::CiphertextIdentityMismatch => {
                SymbolizeError::CiphertextIdentityMismatch
            }
        })?;

    // Layer 2, and the one FG-INV-09 names: the recovered plaintext must
    // recompute the requested identity. The AEAD proves the bytes are the ones
    // that were sealed; this proves they are the object we asked for.
    let mut identity_header = Vec::with_capacity(2 + canonical_header.len());
    identity_header.extend_from_slice(&encoding.cipher_descriptor().object_kind.to_le_bytes());
    identity_header.extend_from_slice(canonical_header);
    let recomputed =
        fgdb_crypto::logical_object_id(k_oid, &namespace.0, &identity_header, &compressed);
    if ObjectId(recomputed.0) != expected_object_id {
        return Err(SymbolizeError::IdentityMismatch);
    }
    Ok(compressed)
}
