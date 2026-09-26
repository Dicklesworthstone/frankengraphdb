//! Request-driven donation of one pinned, authenticated Chronicle object.
//!
//! Source symbols are copied directly from the RFC block/sub-block layout.
//! Repair symbols use a lazily constructed, reusable foundation encoder, not a
//! generated prefix ending at the requested ESI. The same SymbolRecord framing,
//! per-encoding MAC and code seed are used by batch encoding and bonded pulls.
//!
//! This is byte production, not transport, retention or authorization. The host
//! must authenticate the requester and negotiated donor roster, retain the exact
//! root/placement/key closure, and supply a fresh ReplCx/authority checkpoint.
//! Dropping or clearing this cache never releases a durable ownership promise.
//! Native AEAD/hash and one bounded foundation encoder invocation are synchronous;
//! checkpoints surround them but do not preempt their internal CPU work.

use std::collections::BTreeSet;
use std::mem::size_of;

use asupersync::net::atp::channel_bonding::{MAX_STATIC_RESIDUE_DONORS, owns_esi};
use asupersync::raptorq::systematic::{SystematicEncoder, SystematicParams};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};

use crate::identity::{CryptoVerificationSink, EncodedObject, RecoveredObjectError};
use crate::symbol::{HEADER_LEN_V1, SYMBOL_MAC_LEN_V1, SymbolRecord};
use crate::symbolize::blocks::{Layout, repair_encoder};
use crate::symbolize::{MAX_SOURCE_SYMBOLS_PER_BLOCK, RecoveryTarget, SymbolizeError};
use crate::transfer::{DonorId, PullRequest};

const MAX_ESI: u32 = 0x00ff_ffff;

/// Limits belong to one object donation session, including retries and cache
/// release. Encoder memory is a logical retained-payload/vector-layout charge,
/// not an allocator/RSS guarantee. Matrix cells independently bound the native
/// solver: its pinned implementation can hold TWO dense L-by-L matrices plus
/// source/intermediate/RHS vectors. `max_repair_source_symbols` bounds those
/// vectors and CPU shape. Zero repair limits permit systematic-only donation.
#[derive(Clone, Copy, Debug)]
pub struct DonorLimits {
    pub max_protected_bytes: usize,
    pub max_identity_header_bytes: usize,
    pub max_repair_source_symbols: usize,
    pub max_matrix_cells: usize,
    pub max_cached_encoder_bytes: usize,
    pub max_encoder_builds: u32,
    pub max_requests: u64,
    /// Attempted record bytes; a cancelled or failed admitted response is not
    /// refunded. Wrong coordinates consume a request but no record-byte charge.
    pub max_wire_bytes: u64,
    pub max_esi: u32,
}

impl Default for DonorLimits {
    fn default() -> Self {
        Self {
            max_protected_bytes: 64 * 1024 * 1024,
            max_identity_header_bytes: 64 * 1024,
            max_repair_source_symbols: 4096,
            max_matrix_cells: 4 * 1024 * 1024,
            max_cached_encoder_bytes: 32 * 1024 * 1024,
            max_encoder_builds: 64,
            max_requests: 65_536,
            max_wire_bytes: 64 * 1024 * 1024,
            max_esi: MAX_ESI,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DonorUsage {
    pub requests: u64,
    pub charged_wire_bytes: u64,
    /// Includes cancelled/failed builds once preparation has been admitted.
    pub encoder_builds: u32,
    pub cached_encoder_bytes: usize,
    pub cached_blocks: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DonorError<E> {
    InvalidLimits,
    ObjectBudget,
    InvalidObject,
    InvalidDonors,
    ForeignRequest,
    UnownedEsi,
    RequestBudget,
    WireBudget,
    EncoderShapeBudget,
    EncoderCacheBudget,
    EncoderBuildBudget,
    AllocationFailed,
    Encoding(SymbolizeError),
    Control(E),
}

impl<E: core::fmt::Debug> core::fmt::Display for DonorError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis object donation: {self:?}")
    }
}
impl<E: core::fmt::Debug> core::error::Error for DonorError<E> {}

struct CachedBlock {
    block: u32,
    bytes: usize,
    encoder: SystematicEncoder,
}

/// Holds immutable borrowed ciphertext/descriptor/key bytes for one donor's
/// negotiated residue. No caller can replace the source after authentication.
/// There is no Clone: making a fresh session is a new host-admitted budget, not
/// an ordinary timeout retry. Use the SAME donor across request retries.
pub struct BondedDonor<'a> {
    encoding: &'a EncodedObject,
    protected: &'a [u8],
    dek: &'a [u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    donor: DonorId,
    slot: u32,
    donor_count: u32,
    layout: Layout,
    limits: DonorLimits,
    usage: DonorUsage,
    cache: Vec<CachedBlock>,
}

impl core::fmt::Debug for BondedDonor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BondedDonor")
            .field("usage", &self.usage)
            .field("source_and_keys", &"[REDACTED]")
            .finish()
    }
}

impl<'a> BondedDonor<'a> {
    /// Authenticate the COMPLETE protected object before issuing any symbol.
    /// The target's namespace/header/key must originate in the pinned canonical
    /// closure, not a requester claim. A roster is ordered: its position defines
    /// the same fixed residue used by BondedPull; membership is not inferred.
    // Each argument is a distinct pinned authentication input, all of which
    // must be checked before the first symbol; none is optional or derived.
    #[allow(clippy::too_many_arguments)]
    pub fn new<E>(
        encoding: &'a EncodedObject,
        protected: &'a [u8],
        target: RecoveryTarget<'_>,
        dek: &'a [u8; 32],
        donor: DonorId,
        donor_ids: &[DonorId],
        limits: DonorLimits,
        verification: &mut dyn CryptoVerificationSink,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, DonorError<E>> {
        checkpoint().map_err(DonorError::Control)?;
        if limits.max_protected_bytes == 0
            || limits.max_esi > MAX_ESI
            || limits.max_repair_source_symbols > MAX_SOURCE_SYMBOLS_PER_BLOCK
        {
            return Err(DonorError::InvalidLimits);
        }
        if protected.len() > limits.max_protected_bytes
            || target.canonical_header.len() > limits.max_identity_header_bytes
        {
            return Err(DonorError::ObjectBudget);
        }
        if target.protected_len != protected.len() || target.object_id != encoding.object_id() {
            return Err(DonorError::InvalidObject);
        }
        let layout = Layout::new(encoding, protected.len()).map_err(DonorError::Encoding)?;
        if donor_ids.is_empty() || donor_ids.len() > MAX_STATIC_RESIDUE_DONORS as usize {
            return Err(DonorError::InvalidDonors);
        }
        let unique: BTreeSet<_> = donor_ids.iter().copied().collect();
        if unique.len() != donor_ids.len() || unique.contains(&DonorId(0)) {
            return Err(DonorError::InvalidDonors);
        }
        let slot = donor_ids
            .iter()
            .position(|id| *id == donor)
            .ok_or(DonorError::InvalidDonors)?;
        checkpoint().map_err(DonorError::Control)?;
        let plaintext = encoding
            .open_recovered(protected, dek, verification)
            .map_err(|error| {
                DonorError::Encoding(match error {
                    RecoveredObjectError::AuthenticationFailed => {
                        SymbolizeError::AuthenticationFailed
                    }
                    RecoveredObjectError::CiphertextIdentityMismatch => {
                        SymbolizeError::CiphertextIdentityMismatch
                    }
                })
            })?;
        checkpoint().map_err(DonorError::Control)?;
        let length = target
            .canonical_header
            .len()
            .checked_add(2)
            .ok_or(DonorError::ObjectBudget)?;
        let mut header = Vec::new();
        header
            .try_reserve_exact(length)
            .map_err(|_| DonorError::AllocationFailed)?;
        header.extend_from_slice(&encoding.cipher_descriptor().object_kind.to_le_bytes());
        header.extend_from_slice(target.canonical_header);
        let identity =
            fgdb_crypto::logical_object_id(target.k_oid, &target.namespace.0, &header, &plaintext);
        if ObjectId(identity.0) != target.object_id {
            return Err(DonorError::Encoding(SymbolizeError::IdentityMismatch));
        }
        drop(plaintext);
        checkpoint().map_err(DonorError::Control)?;
        let mut cache = Vec::new();
        // At most one encoder per block. The fixed metadata population is
        // bounded by the already checked RFC block count, independent of ESI.
        cache
            .try_reserve_exact(layout.blocks())
            .map_err(|_| DonorError::AllocationFailed)?;
        Ok(Self {
            encoding,
            protected,
            dek,
            namespace: target.namespace,
            donor,
            slot: slot as u32,
            donor_count: donor_ids.len() as u32,
            layout,
            limits,
            usage: DonorUsage::default(),
            cache,
        })
    }

    pub fn usage(&self) -> DonorUsage {
        self.usage
    }
    pub fn namespace(&self) -> DatabaseSecurityNamespaceId {
        self.namespace
    }
    pub fn donor(&self) -> DonorId {
        self.donor
    }
    pub fn record_len(&self) -> usize {
        usize::from(HEADER_LEN_V1)
            + usize::from(self.encoding.descriptor().symbol_size)
            + usize::from(SYMBOL_MAC_LEN_V1)
    }

    /// Explicit physical cache release. No automatic eviction or budget reset:
    /// a later repair request consumes another build, while original source
    /// requests still need no encoder. This releases no prepared/root retention.
    pub fn release_encoder(&mut self, block: u32) -> bool {
        let Some(position) = self.cache.iter().position(|entry| entry.block == block) else {
            return false;
        };
        let entry = self.cache.swap_remove(position);
        self.usage.cached_encoder_bytes -= entry.bytes;
        self.usage.cached_blocks -= 1;
        true
    }

    /// Produce just the requested authenticated record. Out-of-order or repeated
    /// requests are legal and deterministic but EACH consumes request/byte work.
    /// Supply the runtime's fresh authority/cancellation check; it runs again
    /// after the completed MAC, before any bytes are returned to the caller.
    pub fn respond<E>(
        &mut self,
        request: PullRequest,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Vec<u8>, DonorError<E>> {
        if self.usage.requests >= self.limits.max_requests {
            return Err(DonorError::RequestBudget);
        }
        self.usage.requests += 1;
        checkpoint().map_err(DonorError::Control)?;
        if request.donor != self.donor
            || request.object_id != self.encoding.object_id()
            || request.encoding_id != self.encoding.encoding_id()
            || self.layout.source_symbols(request.source_block).is_none()
            || request.esi > self.limits.max_esi
        {
            return Err(DonorError::ForeignRequest);
        }
        if !owns_esi(self.slot, self.donor_count, request.esi) {
            return Err(DonorError::UnownedEsi);
        }
        let charged = self
            .usage
            .charged_wire_bytes
            .checked_add(self.record_len() as u64)
            .filter(|bytes| *bytes <= self.limits.max_wire_bytes)
            .ok_or(DonorError::WireBudget)?;
        self.usage.charged_wire_bytes = charged;
        checkpoint().map_err(DonorError::Control)?;
        let size = usize::from(self.encoding.descriptor().symbol_size);
        let mut symbol = Vec::new();
        symbol
            .try_reserve_exact(size)
            .map_err(|_| DonorError::AllocationFailed)?;
        symbol.resize(size, 0);
        let sources = self
            .layout
            .source_symbols(request.source_block)
            .ok_or(DonorError::ForeignRequest)?;
        if (request.esi as usize) < sources {
            self.layout
                .copy_source_symbol(
                    self.protected,
                    request.source_block,
                    request.esi,
                    &mut symbol,
                )
                .map_err(DonorError::Encoding)?;
        } else {
            let position = self.ensure_encoder(request.source_block, &mut checkpoint)?;
            self.cache[position]
                .encoder
                .try_repair_symbol_into(request.esi, &mut symbol)
                .map_err(|_| DonorError::Encoding(SymbolizeError::InvalidParameters))?;
        }
        checkpoint().map_err(DonorError::Control)?;
        let record =
            SymbolRecord::for_encoding(self.encoding, request.source_block, request.esi, 0, symbol)
                .serialize(&self.encoding.symbol_auth_key(self.dek));
        checkpoint().map_err(DonorError::Control)?;
        Ok(record)
    }

    fn ensure_encoder<E>(
        &mut self,
        block: u32,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<usize, DonorError<E>> {
        if let Some(position) = self.cache.iter().position(|entry| entry.block == block) {
            return Ok(position);
        }
        let k = self
            .layout
            .source_symbols(block)
            .ok_or(DonorError::ForeignRequest)?;
        let size = usize::from(self.encoding.descriptor().symbol_size);
        if k > self.limits.max_repair_source_symbols {
            return Err(DonorError::EncoderShapeBudget);
        }
        let params = SystematicParams::try_for_source_block(k, size)
            .map_err(|_| DonorError::Encoding(SymbolizeError::InvalidParameters))?;
        let cells = params
            .l
            .checked_mul(params.l)
            .ok_or(DonorError::EncoderShapeBudget)?;
        if cells > self.limits.max_matrix_cells {
            return Err(DonorError::EncoderShapeBudget);
        }
        let bytes = params
            .l
            .checked_add(k)
            .and_then(|count| {
                size.checked_add(size_of::<Vec<u8>>())
                    .and_then(|width| count.checked_mul(width))
            })
            .and_then(|bytes| bytes.checked_add(size_of::<SystematicEncoder>()))
            .ok_or(DonorError::EncoderCacheBudget)?;
        let cached = self
            .usage
            .cached_encoder_bytes
            .checked_add(bytes)
            .filter(|sum| *sum <= self.limits.max_cached_encoder_bytes)
            .ok_or(DonorError::EncoderCacheBudget)?;
        if self.usage.encoder_builds >= self.limits.max_encoder_builds {
            return Err(DonorError::EncoderBuildBudget);
        }
        // Charge before preparation. Repeated cancellation/panic cannot grant
        // unbounded source copies or native matrix solves under one session.
        self.usage.encoder_builds += 1;
        checkpoint().map_err(DonorError::Control)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(k)
            .map_err(|_| DonorError::AllocationFailed)?;
        for esi in 0..k {
            checkpoint().map_err(DonorError::Control)?;
            let mut symbol = Vec::new();
            symbol
                .try_reserve_exact(size)
                .map_err(|_| DonorError::AllocationFailed)?;
            symbol.resize(size, 0);
            self.layout
                .copy_source_symbol(self.protected, block, esi as u32, &mut symbol)
                .map_err(DonorError::Encoding)?;
            source.push(symbol);
        }
        checkpoint().map_err(DonorError::Control)?;
        let encoder = repair_encoder(self.encoding, &source).map_err(DonorError::Encoding)?;
        drop(source);
        checkpoint().map_err(DonorError::Control)?;
        let position = self.cache.len();
        self.cache.push(CachedBlock {
            block,
            bytes,
            encoder,
        });
        self.usage.cached_encoder_bytes = cached;
        self.usage.cached_blocks += 1;
        Ok(position)
    }
}
