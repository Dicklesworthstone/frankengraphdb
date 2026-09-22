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
use crate::symbolize::{
    MAX_SOURCE_SYMBOLS_PER_BLOCK, RecoveryTarget, SymbolizeError, decode_object,
};
use asupersync::net::atp::channel_bonding::{
    DonorEsiStream, MAX_STATIC_RESIDUE_DONORS, owns_esi,
};
use fgdb_crypto::Digest;
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DonorId(pub u128);

/// Per-object resource bounds. Stored wire bytes do not include the decoder's
/// workspace: `max_source_symbols` bounds its shape independently.
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
    stream: DonorEsiStream,
    available: bool,
    exhausted: bool,
}

pub struct BondedPull<'a> {
    encoding: &'a EncodedObject,
    target: RecoveryTarget<'a>,
    dek: &'a [u8; 32],
    limits: PullLimits,
    source_symbols: usize,
    record_len: usize,
    donors: Vec<Donor>,
    next_donor: usize,
    pending: BTreeMap<u32, DonorId>,
    seen: BTreeMap<u32, usize>,
    records: Vec<Vec<u8>>,
    stored_bytes: usize,
    requests: u64,
    verifications: u64,
    decode_attempts: u32,
    last_decode_count: usize,
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
    let remaining = limit.checked_sub(used).filter(|remaining| *remaining != 0)
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
        if descriptor.source_block_count != 1 {
            // The current Chronicle decoder is a one-block decoder. Never
            // mix source blocks into a system that discards the block number.
            return Err(PullError::UnsupportedSourceBlocks);
        }
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
        let source_symbols = target.protected_len.div_ceil(symbol_size);
        let record_len = usize::from(HEADER_LEN_V1) + symbol_size + usize::from(SYMBOL_MAC_LEN_V1);
        if limits.max_source_symbols == 0
            || limits.max_source_symbols > MAX_SOURCE_SYMBOLS_PER_BLOCK
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
            || u64::from(limits.max_esi) + 1 < source_symbols as u64
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
        let mut donors = Vec::new();
        donors
            .try_reserve_exact(donor_ids.len())
            .map_err(|_| PullError::AllocationFailed)?;
        for (index, id) in donor_ids.iter().enumerate() {
            donors.push(Donor {
                id: *id,
                stream: DonorEsiStream::new(index as u32, donor_ids.len() as u32)
                    .map_err(|_| PullError::InvalidDonors)?,
                available: true,
                exhausted: false,
            });
        }
        Ok(Self {
            encoding,
            target,
            dek,
            limits,
            source_symbols,
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
            last_decode_count: 0,
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
        // Every pending request reserves one symbol slot and one complete wire
        // record. Admission exchanges that reservation for owned bytes, so all
        // three differences remain nonnegative even when replies are reordered.
        let capacity = maximum
            .min(self.limits.max_in_flight - self.pending.len())
            .min(window.unwrap_or(self.limits.max_in_flight).saturating_sub(self.pending.len()))
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
            self.limits.max_verifications, self.verifications, self.pending.len(),
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
        for donor in self.pending.values() {
            *outstanding.entry(*donor).or_default() += 1;
        }
        let mut caps = BTreeMap::new();
        if let Some(window) = window {
            let window = window.min(self.limits.max_in_flight);
            let available = self.donors.iter().filter(|donor| donor.available).count();
            if available != 0 {
                let quotient = window / available;
                let remainder = window % available;
                for (rank, donor) in self.donors.iter().filter(|donor| donor.available).enumerate() {
                    let extra = usize::from(quotient != 0 && rank < remainder);
                    caps.insert(donor.id, quotient.max(1) + extra);
                }
            }
        }
        for _ in 0..count {
            let mut selected = None;
            let mut throttled = false;
            for _ in 0..self.donors.len() {
                let slot = self.next_donor;
                self.next_donor = (slot + 1) % self.donors.len();
                let donor = &mut self.donors[slot];
                if !donor.available || donor.exhausted {
                    continue;
                }
                let cap = caps.get(&donor.id).copied().unwrap_or(usize::MAX);
                if outstanding.get(&donor.id).copied().unwrap_or(0) >= cap {
                    throttled = true;
                    continue;
                }
                match donor.stream.next() {
                    Some(esi) if esi <= self.limits.max_esi => {
                        selected = Some((donor.id, esi));
                        break;
                    }
                    _ => donor.exhausted = true,
                }
            }
            let Some((donor, esi)) = selected else {
                if !out.is_empty() || throttled {
                    break;
                }
                return Err(if self.donors.iter().any(|donor| donor.available) {
                    PullError::SymbolSpaceExhausted
                } else {
                    PullError::NoAvailableDonor
                });
            };
            self.pending.insert(esi, donor);
            *outstanding.entry(donor).or_default() += 1;
            self.requests += 1;
            out.push(PullRequest {
                donor,
                object_id: self.encoding.object_id(),
                encoding_id: self.encoding.encoding_id(),
                source_block: 0,
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
            || request.source_block != 0
            || self.pending.get(&request.esi) != Some(&request.donor)
        {
            return Err(PullError::UnrequestedSymbol);
        }
        self.pending.remove(&request.esi);
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
            || request.source_block != 0
        {
            return Err(PullError::UnrequestedSymbol);
        }
        self.accept_inner(request.donor, Some(request.esi), bytes, verification)
    }

    fn accept_inner(
        &mut self,
        donor: DonorId,
        expected_esi: Option<u32>,
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
        if record.source_block != 0
            || record.esi > self.limits.max_esi
            || expected_esi.is_some_and(|expected| expected != record.esi)
            || !owns_esi(slot as u32, self.donors.len() as u32, record.esi)
        {
            return Err(PullError::UnrequestedSymbol);
        }
        if let Some(index) = self.seen.get(&record.esi) {
            return if self.records[*index].as_slice() == bytes {
                Ok(SymbolAdmission::Duplicate)
            } else {
                Err(PullError::ConflictingSymbol)
            };
        }
        if self.pending.get(&record.esi) != Some(&donor) {
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
        self.seen.insert(record.esi, self.records.len());
        self.records.push(owned);
        self.stored_bytes += bytes.len();
        self.pending.remove(&record.esi);
        Ok(SymbolAdmission::Added)
    }

    /// Rank deficiency asks for more equations. Every other recovery failure
    /// closes this attempt; corruption is never relabeled as insufficient data.
    /// Repeated polling without another accepted symbol performs no new decode.
    pub fn try_recover(
        &mut self,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Option<VerifiedObject>, PullError> {
        self.open()?;
        if self.records.len() < self.source_symbols || self.records.len() == self.last_decode_count {
            return Ok(None);
        }
        if self.decode_attempts >= self.limits.max_decode_attempts {
            return Err(PullError::DecodeBudget);
        }
        self.decode_attempts += 1;
        self.last_decode_count = self.records.len();
        match decode_object(self.encoding, &self.records, self.target, self.dek, verification) {
            Ok(plaintext) => {
                self.closed = true;
                self.pending.clear();
                Ok(Some(VerifiedObject {
                    namespace: self.target.namespace,
                    encoding: self.encoding.clone(),
                    plaintext,
                }))
            }
            Err(SymbolizeError::InsufficientSymbols) => Ok(None),
            Err(error) => {
                self.closed = true;
                Err(PullError::Recovery(error))
            }
        }
    }
}

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
            assert!(matches!(verification_capacity(limit, used, 0), Err(PullError::VerificationBudget)));
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
