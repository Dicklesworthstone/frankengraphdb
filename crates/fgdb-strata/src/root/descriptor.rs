//! Root-authenticated payload descriptors. Constructors are crate-private:
//! only a successful publication admission may turn them into durable root
//! metadata. A checksum authenticates a later extent read; it does not mint
//! an edge-history or vertex-history validation receipt.

use super::{BlockRef, PatchRef, RootError};
use crate::AdjacencyEntry;
use crate::root_segment::{SegmentClass, SegmentEntry};
use crate::tiered::buffer::{ExtentKey, extent_checksum};
use crate::vertex::VertexRow;
use fgdb_delta_types::RelationId;
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, EId, VId};

pub(crate) const BLOCK_BYTES: usize = 205;
pub(crate) const PATCH_BYTES: usize = 80;
const MAX_PAYLOAD_BYTES: usize = crate::MAX_BLOCK_ENTRIES as usize * 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockSummary {
    pub(crate) first_edge: (EId, CommitSeq),
    pub(crate) last_edge: EId,
    pub(crate) source: VId,
    pub(crate) relation: RelationId,
    pub(crate) first_destination: VId,
    pub(crate) last_destination: VId,
}

impl BlockSummary {
    pub(crate) fn from_entries(entries: &[AdjacencyEntry]) -> Option<Self> {
        let first = entries.first()?;
        Some(Self {
            first_edge: entries.iter().map(|row| (row.eid, row.created_at)).min()?,
            last_edge: entries.iter().map(|row| row.eid).max()?,
            source: first.src,
            relation: first.relation,
            first_destination: first.dst,
            last_destination: entries.last()?.dst,
        })
    }

    pub(crate) fn may_contain_edge(self, eid: EId) -> bool {
        self.first_edge.0 <= eid && eid <= self.last_edge
    }

    pub(crate) fn may_contain_adjacency(
        self,
        endpoint: VId,
        relation: Option<RelationId>,
        incoming: bool,
    ) -> bool {
        relation.is_none_or(|relation| self.relation == relation)
            && if incoming {
                self.first_destination <= endpoint && endpoint <= self.last_destination
            } else {
                self.source == endpoint
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockDescriptor {
    pub(crate) block: ExtentKey,
    pub(crate) properties: Option<ExtentKey>,
    pub(crate) summary: Option<BlockSummary>,
    pub(crate) rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PatchDescriptor {
    pub(crate) extent: ExtentKey,
    pub(crate) first: Option<(VId, CommitSeq)>,
    pub(crate) last: Option<VId>,
    pub(crate) rows: usize,
}

impl PatchDescriptor {
    pub(crate) fn may_contain(self, vid: VId) -> bool {
        self.first
            .zip(self.last)
            .is_some_and(|((first, _), last)| first <= vid && vid <= last)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RootDescriptors {
    pub(crate) blocks: Vec<BlockDescriptor>,
    pub(crate) patches: Vec<PatchDescriptor>,
}

fn invalid() -> RootError {
    RootError::InvalidDescriptor
}

pub(crate) fn extent(id: ObjectId, bytes: &[u8]) -> Result<ExtentKey, RootError> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid());
    }
    ExtentKey::new(id, 0, bytes.len(), extent_checksum(bytes)).map_err(|_| invalid())
}

impl BlockDescriptor {
    pub(crate) fn admitted(
        id: ObjectId,
        bytes: &[u8],
        entries: &[AdjacencyEntry],
        properties: Option<(ObjectId, &[u8])>,
    ) -> Result<Self, RootError> {
        Ok(Self {
            block: extent(id, bytes)?,
            properties: properties
                .map(|(id, bytes)| extent(id, bytes))
                .transpose()?,
            summary: BlockSummary::from_entries(entries),
            rows: entries.len(),
        })
    }
}

impl PatchDescriptor {
    pub(crate) fn admitted(
        id: ObjectId,
        bytes: &[u8],
        rows: &[VertexRow],
    ) -> Result<Self, RootError> {
        Ok(Self {
            extent: extent(id, bytes)?,
            first: rows.first().map(|row| (row.vid, row.created_at)),
            last: rows.last().map(|row| row.vid),
            rows: rows.len(),
        })
    }
}

fn encode_extent(out: &mut Vec<u8>, key: ExtentKey) -> Result<(), RootError> {
    if key.offset() != 0 || key.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid());
    }
    out.extend_from_slice(&(key.len() as u32).to_be_bytes());
    out.extend_from_slice(&key.checksum());
    Ok(())
}

fn encode_block(out: &mut Vec<u8>, value: &BlockDescriptor) -> Result<(), RootError> {
    let summary = value.summary.ok_or_else(invalid)?;
    if value.rows == 0 || value.rows > crate::MAX_BLOCK_ENTRIES as usize {
        return Err(invalid());
    }
    encode_extent(out, value.block)?;
    out.extend_from_slice(&(value.rows as u32).to_be_bytes());
    out.extend_from_slice(&summary.first_edge.0.0.to_be_bytes());
    out.extend_from_slice(&summary.first_edge.1.0.to_be_bytes());
    out.extend_from_slice(&summary.last_edge.0.to_be_bytes());
    out.extend_from_slice(&summary.source.0.to_be_bytes());
    out.extend_from_slice(&summary.relation.0.to_be_bytes());
    out.extend_from_slice(&summary.first_destination.0.to_be_bytes());
    out.extend_from_slice(&summary.last_destination.0.to_be_bytes());
    if let Some(properties) = value.properties {
        out.push(1);
        out.extend_from_slice(&properties.object().0);
        encode_extent(out, properties)?;
    } else {
        out.extend_from_slice(&[0; 69]);
    }
    Ok(())
}

fn encode_patch(out: &mut Vec<u8>, value: &PatchDescriptor) -> Result<(), RootError> {
    let (first, created_at) = value.first.ok_or_else(invalid)?;
    let last = value.last.ok_or_else(invalid)?;
    if value.rows == 0 || value.rows > crate::vertex::MAX_PATCH_ROWS as usize {
        return Err(invalid());
    }
    encode_extent(out, value.extent)?;
    out.extend_from_slice(&(value.rows as u32).to_be_bytes());
    out.extend_from_slice(&first.0.to_be_bytes());
    out.extend_from_slice(&created_at.0.to_be_bytes());
    out.extend_from_slice(&last.0.to_be_bytes());
    Ok(())
}

impl RootDescriptors {
    pub(crate) fn encode_range(
        &self,
        class: SegmentClass,
        range: std::ops::Range<usize>,
        out: &mut Vec<u8>,
    ) -> Result<(), RootError> {
        match class {
            SegmentClass::Blocks => {
                for value in self.blocks.get(range).ok_or_else(invalid)? {
                    encode_block(out, value)?;
                }
            }
            SegmentClass::VertexPatches => {
                for value in self.patches.get(range).ok_or_else(invalid)? {
                    encode_patch(out, value)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn matches_suffix(
        &self,
        blocks: &[BlockRef],
        patches: &[PatchRef],
        carried: (usize, usize),
    ) -> bool {
        self.blocks.len() == blocks.len()
            && self.patches.len() == patches.len()
            && self
                .blocks
                .iter()
                .zip(blocks)
                .skip(carried.0)
                .all(|(d, r)| d.block.object() == r.block_id)
            && self
                .patches
                .iter()
                .zip(patches)
                .skip(carried.1)
                .all(|(d, r)| d.extent.object() == r.patch_id)
    }

    pub(crate) fn append(&mut self, other: Self) {
        self.blocks.extend(other.blocks);
        self.patches.extend(other.patches);
    }
}

struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], RootError> {
        let (head, tail) = self.0.split_at_checked(N).ok_or_else(invalid)?;
        self.0 = tail;
        head.try_into().map_err(|_| invalid())
    }

    fn u32(&mut self) -> Result<u32, RootError> {
        Ok(u32::from_be_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, RootError> {
        Ok(u64::from_be_bytes(self.take()?))
    }
    fn u128(&mut self) -> Result<u128, RootError> {
        Ok(u128::from_be_bytes(self.take()?))
    }

    fn extent(&mut self, id: ObjectId) -> Result<ExtentKey, RootError> {
        let len = self.u32()? as usize;
        let checksum = self.take()?;
        if len > MAX_PAYLOAD_BYTES {
            return Err(invalid());
        }
        ExtentKey::new(id, 0, len, checksum).map_err(|_| invalid())
    }
}

pub(crate) const fn encoded_len(class: SegmentClass, count: usize) -> usize {
    count
        * match class {
            SegmentClass::Blocks => BLOCK_BYTES,
            SegmentClass::VertexPatches => PATCH_BYTES,
        }
}

pub(crate) fn decode(
    class: SegmentClass,
    entries: &[SegmentEntry],
    bytes: &[u8],
) -> Result<RootDescriptors, RootError> {
    if bytes.len() != encoded_len(class, entries.len()) {
        return Err(invalid());
    }
    let mut input = Input(bytes);
    let mut descriptors = RootDescriptors::default();
    for reference in entries {
        let extent = input.extent(reference.id)?;
        let rows = input.u32()? as usize;
        if rows == 0 || rows > crate::MAX_BLOCK_ENTRIES as usize {
            return Err(invalid());
        }
        match class {
            SegmentClass::Blocks => {
                let first_edge = (EId(input.u128()?), CommitSeq(input.u64()?));
                let last_edge = EId(input.u128()?);
                let source = VId(input.u128()?);
                let relation = RelationId(input.u64()?);
                let first_destination = VId(input.u128()?);
                let last_destination = VId(input.u128()?);
                let present = input.take::<1>()?[0];
                let id = ObjectId(input.take()?);
                let properties = match present {
                    0 => {
                        if id != ObjectId([0; 32]) || input.take::<36>()? != [0; 36] {
                            return Err(invalid());
                        }
                        None
                    }
                    1 => Some(input.extent(id)?),
                    _ => return Err(invalid()),
                };
                if first_edge.0 > last_edge
                    || first_destination > last_destination
                    || first_edge.1 < reference.first_seq
                    || first_edge.1 > reference.last_seq
                {
                    return Err(invalid());
                }
                descriptors.blocks.push(BlockDescriptor {
                    block: extent,
                    properties,
                    summary: Some(BlockSummary {
                        first_edge,
                        last_edge,
                        source,
                        relation,
                        first_destination,
                        last_destination,
                    }),
                    rows,
                });
            }
            SegmentClass::VertexPatches => {
                let first = (VId(input.u128()?), CommitSeq(input.u64()?));
                let last = VId(input.u128()?);
                if rows > crate::vertex::MAX_PATCH_ROWS as usize
                    || first.0 > last
                    || first.1 < reference.first_seq
                    || first.1 > reference.last_seq
                {
                    return Err(invalid());
                }
                descriptors.patches.push(PatchDescriptor {
                    extent,
                    first: Some(first),
                    last: Some(last),
                    rows,
                });
            }
        }
    }
    Ok(descriptors)
}
