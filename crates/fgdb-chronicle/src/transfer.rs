//! Bounded ATP bonded pulls over Chronicle's exact authenticated encoding.
//!
//! Donor scheduling uses asupersync's disjoint ESI streams. Losing a donor
//! removes its outstanding requests, not already authenticated symbols. The
//! remaining streams continue into repair ESIs; no physical byte agreement
//! between donors is treated as authority. A transport driver must supply
//! authenticated donor identities and route these in-process requests over
//! ATP under ReplCx. This module creates no new wire format or network runtime.

use crate::identity::{CryptoVerificationSink, EncodedObject};
use crate::symbol::{HEADER_LEN_V1, SYMBOL_MAC_LEN_V1, SymbolError, SymbolRecord};
use crate::symbolize::blocks::{Layout, MAX_SOURCE_BLOCKS};
use crate::symbolize::{MAX_SOURCE_SYMBOLS_PER_BLOCK, RecoveryTarget, SymbolizeError};
use asupersync::net::atp::channel_bonding::{DonorEsiStream, MAX_STATIC_RESIDUE_DONORS, owns_esi};
use fgdb_crypto::Digest;
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};
use std::collections::{BTreeMap, BTreeSet};

mod recovery;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DonorId(pub u128);

/// Per-object resource bounds. Stored wire bytes do not include the decoder's
/// workspace: `max_source_symbols` bounds the total source-symbol population
/// across all blocks, not a separate allowance for every block. Each block
/// additionally obeys the foundation decoder's 56,403-source-symbol ceiling.
/// Reconstruction retains at most `protected_len` additional protected bytes
/// across retries. This is separate from raw-wire storage and native workspace,
/// and bounded by the admitted total source population and symbol size.
#[derive(Clone, Copy, Debug)]
pub struct PullLimits {
    pub max_source_symbols: usize,
    pub max_symbols: usize,
    pub max_in_flight: usize,
    pub max_wire_bytes: usize,
    pub max_identity_header_bytes: usize,
    pub max_requests: u64,
    /// Ingress authentication attempts, including duplicates and bad MACs.
    /// Recovery's additional authentication work is bounded by the product of
    /// `max_decode_attempts` and `max_symbols`.
    pub max_verifications: u64,
    /// Object-wide recovery rounds. A round visits each changed block at most
    /// once; every still-deficient block must gain an equation before a retry.
    pub max_decode_attempts: u32,
    pub max_esi: u32,
}

impl Default for PullLimits {
    fn default() -> Self {
        Self {
            max_source_symbols: 4096,
            max_symbols: 8192,
            max_in_flight: 64,
            max_wire_bytes: 64 * 1024 * 1024,
            max_identity_header_bytes: 64 * 1024,
            max_requests: 65_536,
            max_verifications: 131_072,
            max_decode_attempts: 16,
            max_esi: 0x00ff_ffff,
        }
    }
}

#[derive(Debug)]
pub enum PullError {
    InvalidLimits,
    InvalidTarget,
    UnsupportedSourceBlocks,
    InvalidDonors,
    UnknownDonor,
    NoAvailableDonor,
    SymbolSpaceExhausted,
    RequestBudget,
    VerificationBudget,
    DecodeBudget,
    StorageBudget,
    AllocationFailed,
    RecordLength,
    UnrequestedSymbol,
    ConflictingSymbol,
    Closed,
    Symbol(SymbolError),
    Recovery(SymbolizeError),
}

impl core::fmt::Display for PullError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis bonded pull: {self:?}")
    }
}

impl core::error::Error for PullError {}

/// In-process ATP request coordinates, not a replacement request wire schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub donor: DonorId,
    pub object_id: ObjectId,
    pub encoding_id: Digest,
    pub source_block: u32,
    pub esi: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolAdmission {
    Added,
    Duplicate,
}

/// Only successful authenticated recovery constructs this value. Its bytes
/// have passed symbol MACs, RaptorQ decoding, object AEAD, CiphertextId and the
/// keyed logical ObjectId check, including the security namespace and header.
/// This proves bytes, NOT durable storage, retention, consensus, or read access.
pub struct VerifiedObject {
    namespace: DatabaseSecurityNamespaceId,
    encoding: EncodedObject,
    plaintext: Vec<u8>,
}

impl VerifiedObject {
    pub fn namespace(&self) -> DatabaseSecurityNamespaceId {
        self.namespace
    }

    pub fn object_id(&self) -> ObjectId {
        self.encoding.object_id()
    }

    pub fn encoding(&self) -> &EncodedObject {
        &self.encoding
    }

    /// The recovered compressed plaintext, not a claim of codec decompression.
    pub fn plaintext(&self) -> &[u8] {
        &self.plaintext
    }

    pub fn into_plaintext(self) -> Vec<u8> {
        self.plaintext
    }
}

impl core::fmt::Debug for VerifiedObject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VerifiedObject")
            .field("object_id", &self.object_id())
            .field("encoding_id", &self.encoding.encoding_id())
            .field("bytes", &self.plaintext.len())
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

struct Donor {
    id: DonorId,
    // Each source block is an independent code, with the SAME stable donor
    // residues. Flattening ESI across blocks would skip original equations.
    streams: Vec<Option<DonorEsiStream>>,
    next_block: usize,
    available: bool,
}

pub struct BondedPull<'a> {
    encoding: &'a EncodedObject,
    target: RecoveryTarget<'a>,
    dek: &'a [u8; 32],
    limits: PullLimits,
    block_sources: Vec<usize>,
    block_received: Vec<usize>,
    record_len: usize,
    donors: Vec<Donor>,
    next_donor: usize,
    pending: BTreeMap<(u32, u32), DonorId>,
    seen: BTreeMap<(u32, u32), usize>,
    records: Vec<Vec<u8>>,
    stored_bytes: usize,
    requests: u64,
    verifications: u64,
    decode_attempts: u32,
    recovery: recovery::Recovery,
    closed: bool,
}

impl core::fmt::Debug for BondedPull<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BondedPull")
            .field("object_id", &self.encoding.object_id())
            .field("symbols", &self.records.len())
            .field("pending", &self.pending.len())
            .field("secrets", &"[REDACTED]")
            .finish()
    }
}

// Each pending coordinate needs one ingress authentication attempt. Do not
// issue work whose valid response cannot fit in the remaining verification
// budget. Expiration frees a reservation, but never refunds work already done.
fn verification_capacity(limit: u64, used: u64, pending: usize) -> Result<usize, PullError> {
    let remaining = limit
        .checked_sub(used)
        .filter(|remaining| *remaining != 0)
        .ok_or(PullError::VerificationBudget)?;
    let reserved = u64::try_from(pending).unwrap_or(u64::MAX);
    Ok(usize::try_from(remaining.saturating_sub(reserved)).unwrap_or(usize::MAX))
}

impl<'a> BondedPull<'a> {
    /// `encoding` and the recovery target must originate in the authenticated
    /// root/descriptor chain, never be selected by donor agreement or gossip.
    pub fn new(
        encoding: &'a EncodedObject,
        target: RecoveryTarget<'a>,
        dek: &'a [u8; 32],
        donor_ids: &[DonorId],
        limits: PullLimits,
    ) -> Result<Self, PullError> {
        let descriptor = encoding.descriptor();
        let symbol_size = usize::from(descriptor.symbol_size);
        let declared_len = encoding
            .cipher_descriptor()
            .compressed_len
            .checked_add(u64::from(encoding.cipher_descriptor().object_tag_len));
        if symbol_size == 0
            || target.protected_len == 0
            || target.object_id != encoding.object_id()
            || u64::try_from(target.protected_len).ok() != Some(descriptor.transfer_length)
            || declared_len != Some(descriptor.transfer_length)
        {
            return Err(PullError::InvalidTarget);
        }
        let layout = Layout::new(encoding, target.protected_len).map_err(|_| {
            if descriptor.source_block_count == 1 {
                PullError::InvalidLimits
            } else {
                // Invalid, contradictory or unsupported multi-block OTI is not
                // a second interpretation of this authenticated descriptor.
                PullError::UnsupportedSourceBlocks
            }
        })?;
        let source_symbols = target.protected_len.div_ceil(symbol_size);
        let record_len = usize::from(HEADER_LEN_V1) + symbol_size + usize::from(SYMBOL_MAC_LEN_V1);
        if limits.max_source_symbols == 0
            || limits.max_source_symbols > MAX_SOURCE_SYMBOLS_PER_BLOCK * MAX_SOURCE_BLOCKS
            || source_symbols > limits.max_source_symbols
            || limits.max_symbols < source_symbols
            || limits.max_in_flight == 0
            || limits.max_in_flight > limits.max_symbols
            || limits.max_wire_bytes / record_len < source_symbols
            || target.canonical_header.len() > limits.max_identity_header_bytes
            || limits.max_requests < source_symbols as u64
            || limits.max_verifications < source_symbols as u64
            || limits.max_decode_attempts == 0
            || limits.max_esi > 0x00ff_ffff
            || u64::from(limits.max_esi) + 1
                < layout.source_symbols(0).ok_or(PullError::InvalidTarget)? as u64
        {
            return Err(PullError::InvalidLimits);
        }
        if donor_ids.is_empty() || donor_ids.len() > MAX_STATIC_RESIDUE_DONORS as usize {
            return Err(PullError::InvalidDonors);
        }
        let unique: BTreeSet<_> = donor_ids.iter().copied().collect();
        if unique.len() != donor_ids.len() || unique.contains(&DonorId(0)) {
            return Err(PullError::InvalidDonors);
        }
        let mut block_sources = Vec::new();
        let mut block_received = Vec::new();
        block_sources
            .try_reserve_exact(layout.blocks())
            .map_err(|_| PullError::AllocationFailed)?;
        block_received
            .try_reserve_exact(layout.blocks())
            .map_err(|_| PullError::AllocationFailed)?;
        for block in 0..layout.blocks() {
            block_sources.push(
                layout
                    .source_symbols(block as u32)
                    .ok_or(PullError::InvalidTarget)?,
            );
            block_received.push(0);
        }
        let mut donors = Vec::new();
        donors
            .try_reserve_exact(donor_ids.len())
            .map_err(|_| PullError::AllocationFailed)?;
        for (index, id) in donor_ids.iter().enumerate() {
            let mut streams = Vec::new();
            streams
                .try_reserve_exact(layout.blocks())
                .map_err(|_| PullError::AllocationFailed)?;
            for _ in 0..layout.blocks() {
                streams.push(Some(
                    DonorEsiStream::new(index as u32, donor_ids.len() as u32)
                        .map_err(|_| PullError::InvalidDonors)?,
                ));
            }
            donors.push(Donor {
                id: *id,
                streams,
                next_block: 0,
                available: true,
            });
        }
        Ok(Self {
            encoding,
            target,
            dek,
            limits,
            block_sources,
            block_received,
            record_len,
            donors,
            next_donor: 0,
            pending: BTreeMap::new(),
            seen: BTreeMap::new(),
            records: Vec::new(),
            stored_bytes: 0,
            requests: 0,
            verifications: 0,
            decode_attempts: 0,
            recovery: recovery::Recovery::new(layout.blocks())?,
            closed: false,
        })
    }

    fn open(&self) -> Result<(), PullError> {
        if self.closed {
            Err(PullError::Closed)
        } else {
            Ok(())
        }
    }

    pub fn symbol_count(&self) -> usize {
        self.records.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Authenticated equations retained for one independent source block.
    /// This count is diagnostic, not decoding-rank or durability evidence.
    pub fn block_symbol_count(&self, block: u32) -> Option<usize> {
        self.block_received.get(block as usize).copied()
    }

    /// Reconstructed blocks at their exact current input sets. This is progress
    /// only: the whole-object AEAD and keyed logical identity may still fail.
    pub fn recovered_block_count(&self) -> usize {
        self.recovery.cached_blocks(&self.block_received)
    }

    fn block_targets(&self) -> Result<Vec<usize>, PullError> {
        let mut targets = Vec::new();
        targets
            .try_reserve_exact(self.block_sources.len())
            .map_err(|_| PullError::AllocationFailed)?;
        for (block, sources) in self.block_sources.iter().enumerate() {
            targets.push(
                self.recovery
                    .target(block, *sources, self.block_received[block])
                    .ok_or(PullError::DecodeBudget)?,
            );
        }
        Ok(targets)
    }

    pub fn decode_attempts(&self) -> u32 {
        self.decode_attempts
    }

    /// Number of fixed ESI residue owners, including temporarily failed donors.
    /// Quarantining a donor never renumbers the other donors' streams.
    pub fn donor_count(&self) -> usize {
        self.donors.len()
    }

    pub fn in_flight_limit(&self) -> usize {
        self.limits.max_in_flight
    }

    /// Fair bounded scheduling. Donor slots and their residue classes do not
    /// change when another donor fails; surviving streams produce repair ESIs.
    pub fn schedule(&mut self, maximum: usize) -> Result<Vec<PullRequest>, PullError> {
        self.schedule_inner(maximum, None)
    }

    /// Refill a streaming pull without letting slow donors accumulate every
    /// global request credit. Divide `window` across available donors, including
    /// outstanding requests, distributing remainder slots in stable donor order.
    /// A window smaller than the donor count allows one request per donor and
    /// relies on ordinary round-robin scheduling and request deadlines.
    /// Throttling is not donor failure or ESI exhaustion: a fully occupied
    /// per-donor window returns an empty batch and preserves each ESI stream.
    pub fn schedule_bonded(
        &mut self,
        maximum: usize,
        window: usize,
    ) -> Result<Vec<PullRequest>, PullError> {
        self.open()?;
        if window == 0 {
            return Err(PullError::InvalidLimits);
        }
        self.schedule_inner(maximum, Some(window))
    }

    fn schedule_inner(
        &mut self,
        maximum: usize,
        window: Option<usize>,
    ) -> Result<Vec<PullRequest>, PullError> {
        self.open()?;
        let multiple = self.block_sources.len() > 1;
        let targets = self.block_targets()?;
        if multiple
            && self
                .block_received
                .iter()
                .zip(&targets)
                .all(|(count, target)| count >= target)
        {
            // Give the caller a chance to decode before spending its remaining
            // object-wide storage on already satisfied blocks.
            return Ok(Vec::new());
        }
        // Every pending request reserves one symbol slot and one complete wire
        // record. Admission exchanges that reservation for owned bytes, so all
        // three differences remain nonnegative even when replies are reordered.
        let capacity = maximum
            .min(self.limits.max_in_flight - self.pending.len())
            .min(
                window
                    .unwrap_or(self.limits.max_in_flight)
                    .saturating_sub(self.pending.len()),
            )
            .min(self.limits.max_symbols - self.records.len() - self.pending.len())
            .min(
                (self.limits.max_wire_bytes - self.stored_bytes) / self.record_len
                    - self.pending.len(),
            );
        if capacity == 0 {
            return Ok(Vec::new());
        }
        let budget = self.limits.max_requests - self.requests;
        if budget == 0 {
            return Err(PullError::RequestBudget);
        }
        let authentication = verification_capacity(
            self.limits.max_verifications,
            self.verifications,
            self.pending.len(),
        )?;
        let count = capacity
            .min(usize::try_from(budget).unwrap_or(usize::MAX))
            .min(authentication);
        let mut out = Vec::new();
        out.try_reserve_exact(count)
            .map_err(|_| PullError::AllocationFailed)?;
        // Derived once per refill; no second persistent credit ledger can drift
        // from pending during admission, expiration or donor quarantine.
        let mut outstanding = BTreeMap::<DonorId, usize>::new();
        let mut reserved = Vec::new();
        reserved
            .try_reserve_exact(self.block_received.len())
            .map_err(|_| PullError::AllocationFailed)?;
        reserved.extend_from_slice(&self.block_received);
        for ((block, _), donor) in &self.pending {
            *outstanding.entry(*donor).or_default() += 1;
            reserved[*block as usize] += 1;
        }
        let mut caps = BTreeMap::new();
        if let Some(window) = window {
            let window = window.min(self.limits.max_in_flight);
            let available = self.donors.iter().filter(|donor| donor.available).count();
            if let Some(quotient) = window.checked_div(available) {
                let remainder = window % available;
                for (rank, donor) in self
                    .donors
                    .iter()
                    .filter(|donor| donor.available)
                    .enumerate()
                {
                    let extra = usize::from(quotient != 0 && rank < remainder);
                    caps.insert(donor.id, quotient.max(1) + extra);
                }
            }
        }
        for _ in 0..count {
            let mut selected = None;
            let mut throttled = false;
            // First reserve every block's deficit, so smaller blocks cannot
            // consume the exact total-K storage budget with extra equations.
            // Once deficits are reserved, healthy donors MAY replace pending
            // equations from silent donors. Pending bytes are not received bytes.
            let deficit = multiple
                && reserved
                    .iter()
                    .zip(&targets)
                    .any(|(count, target)| count < target);
            'select: for offset in 0..self.donors.len() {
                let slot = (self.next_donor + offset) % self.donors.len();
                let donor = &mut self.donors[slot];
                if !donor.available {
                    continue;
                }
                let cap = caps.get(&donor.id).copied().unwrap_or(usize::MAX);
                if outstanding.get(&donor.id).copied().unwrap_or(0) >= cap {
                    throttled = true;
                    continue;
                }
                for offset in 0..donor.streams.len() {
                    let block = (donor.next_block + offset) % donor.streams.len();
                    let needs_equation = !multiple
                        || if deficit {
                            reserved[block] < targets[block]
                        } else {
                            self.block_received[block] < targets[block]
                        };
                    if !needs_equation {
                        continue;
                    }
                    // Peek via the foundation's small Clone value; only the
                    // selected stream advances. Prefer original source symbols
                    // when their owners have credit, but never wait for a silent
                    // donor merely because it owns a lower source ESI.
                    let next = donor.streams[block]
                        .as_ref()
                        .and_then(|stream| stream.clone().next());
                    match next {
                        Some(esi) if esi <= self.limits.max_esi => {
                            if !multiple || (esi as usize) < self.block_sources[block] {
                                selected = Some((slot, block, esi));
                                break 'select;
                            }
                            if selected.is_none() {
                                selected = Some((slot, block, esi));
                            }
                        }
                        _ => donor.streams[block] = None,
                    }
                }
            }
            let Some((slot, block, esi)) = selected else {
                if !out.is_empty() || throttled {
                    break;
                }
                return Err(if self.donors.iter().any(|donor| donor.available) {
                    PullError::SymbolSpaceExhausted
                } else {
                    PullError::NoAvailableDonor
                });
            };
            let donor = &mut self.donors[slot];
            let advanced = donor.streams[block].as_mut().and_then(Iterator::next);
            debug_assert_eq!(advanced, Some(esi));
            donor.next_block = (block + 1) % donor.streams.len();
            let id = donor.id;
            self.next_donor = (slot + 1) % self.donors.len();
            self.pending.insert((block as u32, esi), id);
            reserved[block] += 1;
            *outstanding.entry(id).or_default() += 1;
            self.requests += 1;
            out.push(PullRequest {
                donor: id,
                object_id: self.encoding.object_id(),
                encoding_id: self.encoding.encoding_id(),
                source_block: block as u32,
                esi,
            });
        }
        Ok(out)
    }

    /// Mark a failed donor unavailable without throwing away authenticated
    /// contributions. A transport cancellation is not a partial object success.
    pub fn donor_failed(&mut self, id: DonorId) -> Result<(), PullError> {
        self.open()?;
        let donor = self
            .donors
            .iter_mut()
            .find(|donor| donor.id == id)
            .ok_or(PullError::UnknownDonor)?;
        donor.available = false;
        self.pending.retain(|_, owner| *owner != id);
        Ok(())
    }

    /// Resume the same authenticated donor without resetting its ESI stream.
    pub fn donor_available(&mut self, id: DonorId) -> Result<(), PullError> {
        self.open()?;
        self.donors
            .iter_mut()
            .find(|donor| donor.id == id)
            .ok_or(PullError::UnknownDonor)?
            .available = true;
        Ok(())
    }

    /// Expiration abandons only this exact request. Late answers cannot consume
    /// another request's credit. The next schedule asks for a fresh equation.
    pub fn expire(&mut self, request: PullRequest) -> Result<(), PullError> {
        self.open()?;
        if request.object_id != self.encoding.object_id()
            || request.encoding_id != self.encoding.encoding_id()
            || self.pending.get(&(request.source_block, request.esi)) != Some(&request.donor)
        {
            return Err(PullError::UnrequestedSymbol);
        }
        self.pending.remove(&(request.source_block, request.esi));
        Ok(())
    }

    /// Authenticate before deduplication, accounting, or decoder admission.
    /// A bad MAC cannot erase a legitimate in-flight request using forged ESI
    /// header bytes. The caller decides whether to quarantine that donor.
    pub fn accept(
        &mut self,
        donor: DonorId,
        bytes: &[u8],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<SymbolAdmission, PullError> {
        self.accept_inner(donor, None, bytes, verification)
    }

    /// Admit a response to one exact, locally issued request.
    ///
    /// Unlike donor-stream ingress through `accept`, an RPC response must also
    /// match its request's ESI. A correctly authenticated record for another
    /// outstanding request (or an already accepted record) must not consume that
    /// other request's credit or masquerade as this request's successful reply.
    /// The record is authenticated before any admission or credit mutation.
    pub fn accept_reply(
        &mut self,
        request: PullRequest,
        bytes: &[u8],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<SymbolAdmission, PullError> {
        self.open()?;
        if request.object_id != self.encoding.object_id()
            || request.encoding_id != self.encoding.encoding_id()
            || request.source_block as usize >= self.block_sources.len()
        {
            return Err(PullError::UnrequestedSymbol);
        }
        self.accept_inner(
            request.donor,
            Some((request.source_block, request.esi)),
            bytes,
            verification,
        )
    }

    fn accept_inner(
        &mut self,
        donor: DonorId,
        expected: Option<(u32, u32)>,
        bytes: &[u8],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<SymbolAdmission, PullError> {
        self.open()?;
        let slot = self
            .donors
            .iter()
            .position(|entry| entry.id == donor)
            .ok_or(PullError::UnknownDonor)?;
        if bytes.len() != self.record_len {
            return Err(PullError::RecordLength);
        }
        if self.verifications >= self.limits.max_verifications {
            return Err(PullError::VerificationBudget);
        }
        self.verifications += 1;
        let record = SymbolRecord::verify(bytes, self.encoding, self.dek, verification)
            .map_err(PullError::Symbol)?;
        let coordinate = (record.source_block, record.esi);
        if record.source_block as usize >= self.block_sources.len()
            || record.esi > self.limits.max_esi
            || expected.is_some_and(|expected| expected != coordinate)
            || !owns_esi(slot as u32, self.donors.len() as u32, record.esi)
        {
            return Err(PullError::UnrequestedSymbol);
        }
        if let Some(index) = self.seen.get(&coordinate) {
            return if self.records[*index].as_slice() == bytes {
                Ok(SymbolAdmission::Duplicate)
            } else {
                Err(PullError::ConflictingSymbol)
            };
        }
        if self.pending.get(&coordinate) != Some(&donor) {
            return Err(PullError::UnrequestedSymbol);
        }
        if self.records.len() >= self.limits.max_symbols
            || bytes.len() > self.limits.max_wire_bytes - self.stored_bytes
        {
            return Err(PullError::StorageBudget);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| PullError::AllocationFailed)?;
        owned.extend_from_slice(bytes);
        self.records
            .try_reserve(1)
            .map_err(|_| PullError::AllocationFailed)?;
        self.seen.insert(coordinate, self.records.len());
        self.records.push(owned);
        self.block_received[record.source_block as usize] += 1;
        self.recovery.input_changed();
        self.stored_bytes += bytes.len();
        self.pending.remove(&coordinate);
        Ok(SymbolAdmission::Added)
    }

    /// Rank deficiency asks for more equations. Every other recovery failure
    /// closes this attempt; corruption is never relabeled as insufficient data.
    /// Repeated polling without another accepted symbol performs no new decode.
    /// Successful blocks survive rank failures in other blocks. Only changed
    /// blocks are retried, and every remaining deficiency must gain an equation
    /// before another object-wide round is charged. A late extra equation for a
    /// decoded block invalidates that block's cache rather than being ignored.
    pub fn try_recover(
        &mut self,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Option<VerifiedObject>, PullError> {
        loop {
            match self.advance_recovery(verification)? {
                recovery::Advance::Progress => {}
                recovery::Advance::AwaitingSymbols => return Ok(None),
                recovery::Advance::Complete(object) => return Ok(Some(object)),
            }
        }
    }
}

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod verification_budget_tests {
    use super::{PullError, verification_capacity};

    #[test]
    fn overlapping_windows_cannot_double_reserve_verification_work() {
        assert_eq!(verification_capacity(17, 0, 0).unwrap(), 17);
        assert_eq!(verification_capacity(17, 0, 8).unwrap(), 9);
        assert_eq!(verification_capacity(17, 0, 17).unwrap(), 0);
        // Admission consumes one attempt while releasing its pending slot.
        assert_eq!(verification_capacity(17, 8, 9).unwrap(), 0);
        // Cancelling the remaining window releases reservations, not spent work.
        assert_eq!(verification_capacity(17, 8, 0).unwrap(), 9);
    }

    #[test]
    fn exhausted_or_overdrawn_verification_budget_fails_closed() {
        for (limit, used) in [(0, 0), (17, 17), (17, 18)] {
            assert!(matches!(
                verification_capacity(limit, used, 0),
                Err(PullError::VerificationBudget)
            ));
        }
        // Duplicate or unsolicited authentication attempts can consume work
        // while other requests remain pending. Never underflow or issue more.
        assert_eq!(verification_capacity(17, 16, 8).unwrap(), 0);
    }

    #[test]
    fn every_reserved_attempt_is_accounted_for() {
        for limit in 1..64_u64 {
            for used in 0..limit {
                for pending in 0..64_usize {
                    let available = verification_capacity(limit, used, pending).unwrap();
                    assert!(available as u64 <= limit - used);
                    if pending as u64 <= limit - used {
                        assert_eq!(available as u64 + pending as u64 + used, limit);
                    } else {
                        assert_eq!(available, 0);
                    }
                }
            }
        }
    }
}
