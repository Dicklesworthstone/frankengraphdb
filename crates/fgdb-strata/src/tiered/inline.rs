//! Tier-I micro-adjacency: eight stable-ID incidences inside one descriptor.
//!
//! The capacity counts retained incidences, NOT distinct destinations or live
//! edges. Parallel EIDs and historical content versions never disappear merely
//! to make a descriptor fit. Overflow is an explicit promotion requirement.
//! Property locators index the owning generation's sidecar; this scalar payload
//! does not claim property-table, branch or snapshot authority by itself.

use core::fmt;
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, VId};
use crate::{AdjacencyEntry, BlockError, DescriptorKey, Direction};

pub const INLINE_CAPACITY: usize = 8;
const HEADER_BYTES: usize = 16 + 8 + 1 + 1;
const SLOT_BYTES: usize = 16 + 16 + 8 + 8 + 4;
pub const MAX_INLINE_PAYLOAD_BYTES: usize = HEADER_BYTES + INLINE_CAPACITY * SLOT_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InlineIncidence {
    pub dst: VId,
    pub eid: EId,
    pub created_at: CommitSeq,
    pub retired_at: Option<CommitSeq>,
    /// Zero means no properties. Nonzero is local to the owning sidecar.
    pub property_locator: u32,
}

impl InlineIncidence {
    fn key(self) -> (VId, EId, CommitSeq) {
        (self.dst, self.eid, self.created_at)
    }

    pub fn entry(self, descriptor: DescriptorKey) -> AdjacencyEntry {
        AdjacencyEntry {
            src: descriptor.src,
            relation: descriptor.relation,
            dst: self.dst,
            eid: self.eid,
            created_at: self.created_at,
            retired_at: self.retired_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InlineError {
    PromotionRequired { incidences: usize },
    Length { expected: usize, found: usize },
    UnsupportedDirection { tag: u8 },
    InvalidEntry(BlockError),
    NonCanonicalOrder { at: usize },
}

impl fmt::Display for InlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "inline adjacency: {self:?}")
    }
}

impl std::error::Error for InlineError {}

/// Fixed-size immutable descriptor payload. No Vec, allocation, raw ordinal,
/// unsafe interior pointer, or hidden delta buffer lives inside this value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineAdjacency {
    descriptor: DescriptorKey,
    slots: [Option<InlineIncidence>; INLINE_CAPACITY],
    len: u8,
}

impl InlineAdjacency {
    pub fn try_new(descriptor: DescriptorKey, entries: &[InlineIncidence]) -> Result<Self, InlineError> {
        if entries.len() > INLINE_CAPACITY {
            return Err(InlineError::PromotionRequired { incidences: entries.len() });
        }
        let mut slots = [None; INLINE_CAPACITY];
        for (at, &slot) in entries.iter().enumerate() {
            crate::validate_entry(at, &slot.entry(descriptor)).map_err(InlineError::InvalidEntry)?;
            if at > 0 && entries[at - 1].key() >= slot.key() {
                return Err(InlineError::NonCanonicalOrder { at });
            }
            slots[at] = Some(slot);
        }
        Ok(Self { descriptor, slots, len: entries.len() as u8 })
    }

    pub const fn descriptor(&self) -> DescriptorKey {
        self.descriptor
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn slots(&self) -> impl DoubleEndedIterator<Item = InlineIncidence> + ExactSizeIterator + '_ {
        self.slots[..self.len()].iter().map(|slot| slot.expect("initialized inline prefix"))
    }

    /// The enclosing read view must authorize this sequence and its lineage.
    pub fn visible_at(&self, as_of: CommitSeq) -> impl Iterator<Item = InlineIncidence> + '_ {
        self.slots().filter(move |slot| slot.entry(self.descriptor).visible_at(as_of))
    }

    pub const fn encoded_len(&self) -> usize {
        HEADER_BYTES + self.len as usize * SLOT_BYTES
    }

    /// Write the exact canonical prefix into caller-owned storage. Failure for
    /// insufficient output capacity leaves the output untouched.
    pub fn encode_into(&self, output: &mut [u8]) -> Result<usize, InlineError> {
        let size = self.encoded_len();
        if output.len() < size {
            return Err(InlineError::Length { expected: size, found: output.len() });
        }
        output[..16].copy_from_slice(&self.descriptor.src.0.to_le_bytes());
        output[16..24].copy_from_slice(&self.descriptor.relation.0.to_le_bytes());
        output[24] = self.descriptor.direction as u8;
        output[25] = self.len;
        let mut at = HEADER_BYTES;
        for slot in self.slots() {
            output[at..at + 16].copy_from_slice(&slot.dst.0.to_le_bytes());
            output[at + 16..at + 32].copy_from_slice(&slot.eid.0.to_le_bytes());
            output[at + 32..at + 40].copy_from_slice(&slot.created_at.0.to_le_bytes());
            output[at + 40..at + 48].copy_from_slice(&slot.retired_at.map_or(0, |seq| seq.0).to_le_bytes());
            output[at + 48..at + 52].copy_from_slice(&slot.property_locator.to_le_bytes());
            at += SLOT_BYTES;
        }
        Ok(size)
    }

    /// Decode the exact payload. The enclosing object owns version framing;
    /// unsupported direction tags, padding tails and invalid intervals refuse.
    pub fn decode(bytes: &[u8]) -> Result<Self, InlineError> {
        if bytes.len() < HEADER_BYTES {
            return Err(InlineError::Length { expected: HEADER_BYTES, found: bytes.len() });
        }
        let len = usize::from(bytes[25]);
        if len > INLINE_CAPACITY {
            return Err(InlineError::PromotionRequired { incidences: len });
        }
        let expected = HEADER_BYTES + len * SLOT_BYTES;
        if bytes.len() != expected {
            return Err(InlineError::Length { expected, found: bytes.len() });
        }
        let descriptor = DescriptorKey {
            src: VId(u128::from_le_bytes(bytes[..16].try_into().expect("bounded header"))),
            relation: RelationId(u64::from_le_bytes(bytes[16..24].try_into().expect("bounded header"))),
            direction: match bytes[24] {
                0 => Direction::Outbound,
                tag => return Err(InlineError::UnsupportedDirection { tag }),
            },
        };
        let empty = InlineIncidence {
            dst: VId(0), eid: EId(0), created_at: CommitSeq(1), retired_at: None, property_locator: 0,
        };
        let mut slots = [empty; INLINE_CAPACITY];
        for (index, slot) in slots[..len].iter_mut().enumerate() {
            let at = HEADER_BYTES + index * SLOT_BYTES;
            let retired = u64::from_le_bytes(bytes[at + 40..at + 48].try_into().expect("bounded slot"));
            *slot = InlineIncidence {
                dst: VId(u128::from_le_bytes(bytes[at..at + 16].try_into().expect("bounded slot"))),
                eid: EId(u128::from_le_bytes(bytes[at + 16..at + 32].try_into().expect("bounded slot"))),
                created_at: CommitSeq(u64::from_le_bytes(bytes[at + 32..at + 40].try_into().expect("bounded slot"))),
                retired_at: (retired != 0).then_some(CommitSeq(retired)),
                property_locator: u32::from_le_bytes(bytes[at + 48..at + 52].try_into().expect("bounded slot")),
            };
        }
        Self::try_new(descriptor, &slots[..len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> DescriptorKey {
        DescriptorKey { src: VId(u128::MAX), relation: RelationId(3), direction: Direction::Outbound }
    }

    fn slot(eid: u128, created: u64, retired: Option<u64>) -> InlineIncidence {
        InlineIncidence { dst: VId(7), eid: EId(eid), created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq), property_locator: eid as u32 }
    }

    #[test]
    fn parallel_edges_versions_and_properties_survive_inline_round_trip() {
        let row = InlineAdjacency::try_new(descriptor(), &[slot(1, 1, Some(5)), slot(1, 5, None), slot(2, 2, None)]).unwrap();
        let mut bytes = [0; MAX_INLINE_PAYLOAD_BYTES];
        let len = row.encode_into(&mut bytes).unwrap();
        assert_eq!(InlineAdjacency::decode(&bytes[..len]).unwrap(), row);
        assert_eq!(row.visible_at(CommitSeq(4)).map(|s| (s.eid.0, s.created_at.0)).collect::<Vec<_>>(), vec![(1, 1), (2, 2)]);
        assert_eq!(row.visible_at(CommitSeq(5)).map(|s| (s.eid.0, s.created_at.0)).collect::<Vec<_>>(), vec![(1, 5), (2, 2)]);
    }

    #[test]
    fn ninth_incidence_requires_promotion_even_for_one_destination() {
        let slots: Vec<_> = (0..9).map(|eid| slot(eid, 1, None)).collect();
        assert!(InlineAdjacency::try_new(descriptor(), &slots[..8]).is_ok());
        assert!(matches!(InlineAdjacency::try_new(descriptor(), &slots), Err(InlineError::PromotionRequired { incidences: 9 })));
    }

    #[test]
    fn malformed_order_lifetime_and_direction_refuse() {
        assert!(InlineAdjacency::try_new(descriptor(), &[slot(2, 1, None), slot(1, 1, None)]).is_err());
        assert!(InlineAdjacency::try_new(descriptor(), &[slot(1, 0, None)]).is_err());
        assert!(InlineAdjacency::try_new(descriptor(), &[slot(1, 5, Some(5))]).is_err());
        let row = InlineAdjacency::try_new(descriptor(), &[]).unwrap();
        let mut bytes = [0; HEADER_BYTES];
        row.encode_into(&mut bytes).unwrap();
        bytes[24] = 1;
        assert!(matches!(InlineAdjacency::decode(&bytes), Err(InlineError::UnsupportedDirection { tag: 1 })));
    }

    #[test]
    fn exact_framing_and_insufficient_output_are_failure_atomic() {
        let row = InlineAdjacency::try_new(descriptor(), &[slot(1, 1, None)]).unwrap();
        let mut short = [99; HEADER_BYTES];
        assert!(row.encode_into(&mut short).is_err());
        assert_eq!(short, [99; HEADER_BYTES]);
        let mut bytes = [0; MAX_INLINE_PAYLOAD_BYTES];
        let len = row.encode_into(&mut bytes).unwrap();
        for at in 0..len {
            assert!(InlineAdjacency::decode(&bytes[..at]).is_err());
        }
        assert!(InlineAdjacency::decode(&bytes[..len + 1]).is_err());
    }
}
