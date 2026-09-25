//! Exact, bounded systematic-symbol payload recoverability (plan §14.1–14.2).
//!
//! This is the calculation inside an availability verifier, NOT a receipt codec,
//! signature verifier or certificate issuer. Inputs are immutable projections of
//! the canonical inventory, configuration and receipts. The caller must verify
//! their complete signed bodies, stable prepared ownership, current fences, key
//! openers and FormNewProposal freshness separately. A successful calculation
//! must never be used as proof of those facts or as permission to release bytes.
//!
//! The supported rule is explicit: every REQUIRED encoding retains all of its
//! systematic source symbols after any `f` configured failure domains disappear.
//! Compatible placements may contribute overlapping ranges; different objects,
//! encodings and source blocks never pool symbols. Repair-symbol counts do not
//! prove RaptorQ rank and are rejected by this rule, not silently treated as MDS.
//! Joint storage sets pass independently; consensus voters are not storage proof.
//!
//! We enumerate maximal failure cuts, not symbol combinations. Coverage is
//! monotone under failures, so checking every size-f cut also proves every
//! smaller cut. Overlapping rack/zone/host domains are supported: failing any
//! domain of a placement removes that placement. Coordinate space is represented
//! by intervals, never a bitmap proportional to a claimed source-symbol count.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use fgdb_order::{Domain, MemberId};
use fgdb_types::ObjectId;

pub mod proposal;

// RFC 6330 §5.3: the systematic source-block bound, NOT the 24-bit ESI bound.
const MAX_SYSTEMATIC_SYMBOLS: u32 = 56_403;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadBasis {
    pub domain: Domain,
    pub configuration: [u8; 32],
    pub predicate_digest: [u8; 32],
    pub base_closure_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FailureDomain(pub u128);

/// Exact placement-to-failure-domain mapping from the authenticated policy.
/// Include shared host/zone domains when their failures are part of the model.
/// A donor cannot choose or replace its own domain mapping in a receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageLocation {
    pub member: MemberId,
    pub placement_id: [u8; 32],
    pub failure_domains: Vec<FailureDomain>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageSets {
    Stable(Vec<MemberId>),
    Joint {
        old: Vec<MemberId>,
        new: Vec<MemberId>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageSide {
    Stable,
    Old,
    New,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailabilityPolicy {
    pub basis: PayloadBasis,
    pub storage_sets: StorageSets,
    /// Strictly increasing placement IDs; every placement has one storage owner.
    pub locations: Vec<StorageLocation>,
    pub tolerated_domain_failures: usize,
}

/// The canonical inventory decides which encodings are REQUIRED. Listing two
/// encodings requires BOTH, not a silently inferred alternative-encoding rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodingRequirement {
    pub object_id: ObjectId,
    pub encoding_id: [u8; 32],
    /// Contiguous source-block ordinals, with each block's exact source count.
    pub source_symbols: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCoverage {
    pub object_id: ObjectId,
    pub encoding_id: [u8; 32],
    pub placement_id: [u8; 32],
    pub source_block: u32,
    /// Half-open interval of systematic ESIs. Empty or repair ranges reject.
    pub first_esi: u32,
    pub end_esi: u32,
}

/// A projection, not an authenticated receipt type. Exactly one receipt per
/// member is admitted; select its canonical current promise before calculation.
/// The authority verifier must bind ALL these facts to that receipt's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptCoverage {
    pub receipt_id: [u8; 32],
    pub basis: PayloadBasis,
    pub storage_member: MemberId,
    pub prepared_ownership_id: [u8; 32],
    pub coverage: Vec<SourceCoverage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailabilityInput {
    pub policy: AvailabilityPolicy,
    /// Strictly increasing (ObjectId, EncodingId), with globally unique encodings.
    pub requirements: Vec<EncodingRequirement>,
    pub receipts: Vec<ReceiptCoverage>,
}

#[derive(Clone, Copy, Debug)]
pub struct AvailabilityLimits {
    pub max_requirements: usize,
    pub max_source_blocks: usize,
    pub max_spans: usize,
    /// Locations, memberships and domain assignments, counted before copying.
    pub max_policy_items: usize,
    /// Total maximal cuts across both independently checked joint storage sets.
    pub max_failure_cases: u64,
    pub max_work: u64,
}

impl Default for AvailabilityLimits {
    fn default() -> Self {
        Self {
            max_requirements: 1024,
            max_source_blocks: 4096,
            max_spans: 65_536,
            max_policy_items: 16_384,
            max_failure_cases: 4096,
            max_work: 4_000_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageGap {
    pub side: StorageSide,
    pub failed_domains: Vec<FailureDomain>,
    pub object_id: ObjectId,
    pub encoding_id: [u8; 32],
    pub source_block: u32,
    pub first_missing_esi: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AvailabilityError<E> {
    InvalidLimits,
    InputBudget,
    InvalidPolicy,
    InvalidInventory,
    DuplicateReceipt,
    WrongBasis,
    UnknownMember,
    UnknownEncoding,
    UnknownPlacement,
    InvalidCoverage,
    FailureCaseBudget,
    WorkBudget,
    AllocationFailed,
    Interrupted(E),
    Unrecoverable(CoverageGap),
}

impl<E: core::fmt::Debug> core::fmt::Display for AvailabilityError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis payload availability: {self:?}")
    }
}
impl<E: core::fmt::Debug> core::error::Error for AvailabilityError<E> {}

/// A calculation tied to this exact immutably borrowed input. No signature,
/// freshness, durable-ownership, proposal or retirement authority is implied.
#[derive(Debug)]
pub struct SystematicAssessment<'a> {
    input: &'a AvailabilityInput,
    checked_failure_cases: u64,
    work: u64,
}

impl SystematicAssessment<'_> {
    pub fn input(&self) -> &AvailabilityInput {
        self.input
    }
    pub fn checked_failure_cases(&self) -> u64 {
        self.checked_failure_cases
    }
    pub fn work(&self) -> u64 {
        self.work
    }
}

struct Work<'a, F> {
    used: u64,
    limit: u64,
    checkpoint: &'a mut F,
}
impl<E, F: FnMut() -> Result<(), E>> Work<'_, F> {
    fn step(&mut self) -> Result<(), AvailabilityError<E>> {
        if self.used >= self.limit {
            return Err(AvailabilityError::WorkBudget);
        }
        if self.used.is_multiple_of(128) {
            (self.checkpoint)().map_err(AvailabilityError::Interrupted)?;
        }
        self.used += 1;
        Ok(())
    }
}

fn reserve<T, E>(vec: &mut Vec<T>, count: usize) -> Result<(), AvailabilityError<E>> {
    vec.try_reserve_exact(count)
        .map_err(|_| AvailabilityError::AllocationFailed)
}
fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}
fn key(requirement: &EncodingRequirement) -> ([u8; 32], [u8; 32]) {
    (requirement.object_id.0, requirement.encoding_id)
}

fn storage_groups(sets: &StorageSets) -> Vec<(StorageSide, &[MemberId])> {
    match sets {
        StorageSets::Stable(members) => vec![(StorageSide::Stable, members)],
        StorageSets::Joint { old, new } => vec![(StorageSide::Old, old), (StorageSide::New, new)],
    }
}

/// Number of maximal cuts. n <= 64, so u128 also bounds intermediates.
fn cut_count(n: usize, k: usize) -> u128 {
    let k = k.min(n - k);
    let mut count = 1_u128;
    for i in 0..k {
        count = count * (n - i) as u128 / (i + 1) as u128;
    }
    count
}

/// Increasing fixed-cardinality masks, including the unique empty cut.
struct Cuts {
    n: usize,
    indices: Vec<usize>,
    done: bool,
}
impl Cuts {
    fn new(n: usize, k: usize) -> Self {
        Self {
            n,
            indices: (0..k).collect(),
            done: false,
        }
    }
}
impl Iterator for Cuts {
    type Item = u64;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mask = self.indices.iter().fold(0, |mask, i| mask | (1_u64 << *i));
        let k = self.indices.len();
        match (0..k).rev().find(|i| self.indices[*i] < self.n - k + *i) {
            None => self.done = true,
            Some(i) => {
                self.indices[i] += 1;
                for j in i + 1..k {
                    self.indices[j] = self.indices[j - 1] + 1;
                }
            }
        }
        Some(mask)
    }
}

struct Block {
    requirement: usize,
    ordinal: u32,
    symbols: u32,
}
// (flat block, first ESI, end ESI, location). Derived lexicographic order is
// deterministic and gives a single-pass interval union for every fault cut.
type Span = (usize, u32, u32, usize);

/// Check complete systematic recoverability without performing I/O or changing
/// any ownership. `checkpoint` is invoked throughout preparation and evaluation,
/// including heap ordering. Rejection/cancellation never returns a partial proof.
/// Hard ceilings prevent a caller-supplied limit from enabling exponential work.
pub fn assess_systematic<'a, E, F: FnMut() -> Result<(), E>>(
    input: &'a AvailabilityInput,
    limits: AvailabilityLimits,
    checkpoint: &mut F,
) -> Result<SystematicAssessment<'a>, AvailabilityError<E>> {
    if limits.max_requirements == 0
        || limits.max_requirements > 65_536
        || limits.max_source_blocks == 0
        || limits.max_source_blocks > 65_536
        || limits.max_spans == 0
        || limits.max_spans > 1_048_576
        || limits.max_policy_items == 0
        || limits.max_policy_items > 65_536
        || limits.max_failure_cases == 0
        || limits.max_failure_cases > 65_536
        || limits.max_work == 0
    {
        return Err(AvailabilityError::InvalidLimits);
    }
    let policy = &input.policy;
    let groups = storage_groups(&policy.storage_sets);
    if input.requirements.is_empty()
        || input.requirements.len() > limits.max_requirements
        || input.receipts.len() > 1024
        || policy.locations.is_empty()
        || policy.locations.len() > 1024
    {
        return Err(AvailabilityError::InputBudget);
    }
    let mut work = Work {
        used: 0,
        limit: limits.max_work,
        checkpoint,
    };
    let mut policy_items = policy.locations.len();
    let mut block_count = 0_usize;
    let mut span_count = 0_usize;
    // Admission by lengths precedes traversal/allocation of nested populations.
    for (_, members) in &groups {
        work.step()?;
        policy_items = policy_items
            .checked_add(members.len())
            .ok_or(AvailabilityError::InputBudget)?;
    }
    for location in &policy.locations {
        work.step()?;
        policy_items = policy_items
            .checked_add(location.failure_domains.len())
            .ok_or(AvailabilityError::InputBudget)?;
    }
    for requirement in &input.requirements {
        work.step()?;
        block_count = block_count
            .checked_add(requirement.source_symbols.len())
            .ok_or(AvailabilityError::InputBudget)?;
    }
    for receipt in &input.receipts {
        work.step()?;
        span_count = span_count
            .checked_add(receipt.coverage.len())
            .ok_or(AvailabilityError::InputBudget)?;
    }
    if policy_items > limits.max_policy_items
        || block_count > limits.max_source_blocks
        || span_count > limits.max_spans
    {
        return Err(AvailabilityError::InputBudget);
    }
    for (_, members) in &groups {
        if members.is_empty()
            || members.len() > 1024
            || !strictly_sorted(members)
            || members.iter().any(|member| member.0 == 0)
        {
            return Err(AvailabilityError::InvalidPolicy);
        }
    }
    let mut domains = BTreeSet::new();
    let mut prior_placement = None;
    for location in &policy.locations {
        work.step()?;
        if prior_placement.is_some_and(|prior| prior >= location.placement_id)
            || !groups
                .iter()
                .any(|(_, members)| members.binary_search(&location.member).is_ok())
            || location.failure_domains.is_empty()
            || !strictly_sorted(&location.failure_domains)
        {
            return Err(AvailabilityError::InvalidPolicy);
        }
        prior_placement = Some(location.placement_id);
        for domain in &location.failure_domains {
            work.step()?;
            domains.insert(*domain);
            if domains.len() > 64 {
                return Err(AvailabilityError::InvalidPolicy);
            }
        }
    }
    let domains: Vec<_> = domains.into_iter().collect();
    let f = policy.tolerated_domain_failures;
    if f >= domains.len() {
        return Err(AvailabilityError::InvalidPolicy);
    }
    let cases = cut_count(domains.len(), f) * groups.len() as u128;
    if cases > u128::from(limits.max_failure_cases) {
        return Err(AvailabilityError::FailureCaseBudget);
    }
    let mut location_masks = Vec::new();
    reserve(&mut location_masks, policy.locations.len())?;
    for location in &policy.locations {
        let mut mask = 0_u64;
        for domain in &location.failure_domains {
            work.step()?;
            let bit = domains
                .binary_search(domain)
                .map_err(|_| AvailabilityError::InvalidPolicy)?;
            mask |= 1_u64 << bit;
        }
        location_masks.push(mask);
    }
    let mut blocks = Vec::new();
    let mut starts = Vec::new();
    reserve(&mut blocks, block_count)?;
    reserve(&mut starts, input.requirements.len())?;
    let mut encodings = BTreeSet::new();
    let mut previous = None;
    for (r, requirement) in input.requirements.iter().enumerate() {
        work.step()?;
        if requirement.source_symbols.is_empty()
            || requirement.source_symbols.len() > 256
            || previous.is_some_and(|old| old >= key(requirement))
            || !encodings.insert(requirement.encoding_id)
        {
            return Err(AvailabilityError::InvalidInventory);
        }
        previous = Some(key(requirement));
        starts.push(blocks.len());
        for (ordinal, symbols) in requirement.source_symbols.iter().copied().enumerate() {
            work.step()?;
            if symbols == 0 || symbols > MAX_SYSTEMATIC_SYMBOLS {
                return Err(AvailabilityError::InvalidInventory);
            }
            blocks.push(Block {
                requirement: r,
                ordinal: ordinal as u32,
                symbols,
            });
        }
    }
    let mut heap = BinaryHeap::<Reverse<Span>>::new();
    heap.try_reserve_exact(span_count)
        .map_err(|_| AvailabilityError::AllocationFailed)?;
    let mut receipt_ids = BTreeSet::new();
    let mut receipt_members = BTreeSet::new();
    for receipt in &input.receipts {
        work.step()?;
        if receipt.basis != policy.basis {
            return Err(AvailabilityError::WrongBasis);
        }
        if !receipt_ids.insert(receipt.receipt_id)
            || !receipt_members.insert(receipt.storage_member)
        {
            return Err(AvailabilityError::DuplicateReceipt);
        }
        if !groups
            .iter()
            .any(|(_, members)| members.binary_search(&receipt.storage_member).is_ok())
        {
            return Err(AvailabilityError::UnknownMember);
        }
        for span in &receipt.coverage {
            work.step()?;
            let r = input
                .requirements
                .binary_search_by_key(&(span.object_id.0, span.encoding_id), key)
                .map_err(|_| AvailabilityError::UnknownEncoding)?;
            let requirement = &input.requirements[r];
            let symbols = requirement
                .source_symbols
                .get(span.source_block as usize)
                .ok_or(AvailabilityError::InvalidCoverage)?;
            if span.first_esi >= span.end_esi || span.end_esi > *symbols {
                return Err(AvailabilityError::InvalidCoverage);
            }
            let location = policy
                .locations
                .binary_search_by_key(&span.placement_id, |location| location.placement_id)
                .map_err(|_| AvailabilityError::UnknownPlacement)?;
            if policy.locations[location].member != receipt.storage_member {
                return Err(AvailabilityError::UnknownPlacement);
            }
            heap.push(Reverse((
                starts[r] + span.source_block as usize,
                span.first_esi,
                span.end_esi,
                location,
            )));
        }
    }
    let mut spans = Vec::new();
    reserve(&mut spans, span_count)?;
    while let Some(Reverse(span)) = heap.pop() {
        work.step()?;
        if spans.last() != Some(&span) {
            spans.push(span);
        }
    }
    // Fixed ranges into the sorted span vector. Missing blocks own empty slices.
    let mut ranges = Vec::new();
    reserve(&mut ranges, blocks.len())?;
    let mut end = 0;
    for block in 0..blocks.len() {
        work.step()?;
        let start = end;
        while end < spans.len() && spans[end].0 == block {
            work.step()?;
            end += 1;
        }
        ranges.push(start..end);
    }
    for (side, members) in groups {
        let eligible: Vec<_> = policy
            .locations
            .iter()
            .map(|location| members.binary_search(&location.member).is_ok())
            .collect();
        for failed in Cuts::new(domains.len(), f) {
            work.step()?;
            for (block, range) in blocks.iter().zip(&ranges) {
                work.step()?;
                let mut covered = 0;
                for &(_, first, last, location) in &spans[range.clone()] {
                    work.step()?;
                    if !eligible[location] || location_masks[location] & failed != 0 {
                        continue;
                    }
                    if first > covered {
                        break;
                    }
                    covered = covered.max(last);
                    if covered == block.symbols {
                        break;
                    }
                }
                if covered != block.symbols {
                    let requirement = &input.requirements[block.requirement];
                    return Err(AvailabilityError::Unrecoverable(CoverageGap {
                        side,
                        failed_domains: domains
                            .iter()
                            .enumerate()
                            .filter_map(|(bit, domain)| {
                                (failed & (1_u64 << bit) != 0).then_some(*domain)
                            })
                            .collect(),
                        object_id: requirement.object_id,
                        encoding_id: requirement.encoding_id,
                        source_block: block.ordinal,
                        first_missing_esi: covered,
                    }));
                }
            }
        }
    }
    (work.checkpoint)().map_err(AvailabilityError::Interrupted)?;
    Ok(SystematicAssessment {
        input,
        checked_failure_cases: cases as u64,
        work: work.used,
    })
}

#[cfg(test)]
mod tests;
