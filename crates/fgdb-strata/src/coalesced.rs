//! Physical seal envelopes containing unchanged V7 blocks and their hosted patches.
//!
//! FGSC V1.0 is `magic[4], major:u16, minor:u16, count:u32, total_len:u64`,
//! followed by ordered `block_len:u32, patch_len:u32, block, patch` members.
//! Envelope integers are little-endian; inner formats retain their own encoding.
//! A zero patch length means absence. Both faces enforce the existing block and
//! patch laws, including keyed patch identity, locator bijection and joint digest.
//!
//! Publication order is supplied by the writer, not sorted here. Repeated families
//! are valid (a family can seal several chunks). Every member has the same partition.
//! There is no seal sequence inferred from entry creation times: retirement and
//! compaction can publish older entries. Admission must still verify membership in
//! the committed seal, predecessor chains, root identities and visibility. This
//! physical envelope introduces no logical object kind or identity transcript.

use crate::edge_props::{
    EdgePropertyPatchError, EdgePropertyPatchVersion, read_property_patch,
    validate_block_patch_consistency,
};
use crate::{
    BlockError, block_logical_digest, decode_block_with_properties, header_partition_and_digest,
};
use fgdb_types::ids::DatabaseSecurityNamespaceId;

pub const COALESCED_MAGIC: [u8; 4] = *b"FGSC";
pub const COALESCED_FORMAT_V1: u16 = 1;
pub const COALESCED_FORMAT_MINOR: u16 = 0;
pub const COALESCED_HEADER_LEN: usize = 4 + 2 + 2 + 4 + 8;
pub const COALESCED_ENTRY_FRAME_LEN: usize = 4 + 4;
/// Materialization bounds, not a limit on the size of a transaction. Larger
/// seals can publish several envelopes without changing member identities.
pub const MAX_COALESCED_BYTES: usize = (crate::store::MAX_STORED_OBJECT_BYTES as usize) * 8;
pub const MAX_COALESCED_MEMBER_BYTES: usize = crate::store::MAX_STORED_OBJECT_BYTES as usize;
pub const MAX_COALESCED_MEMBERS: usize =
    (MAX_COALESCED_BYTES - COALESCED_HEADER_LEN) / (COALESCED_ENTRY_FRAME_LEN + 1);

/// Immutable member views borrow the original bytes, preserving predecessor
/// metadata, patch identities and locators without re-encoding or copying them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoalescedBlock<'a> {
    pub block_bytes: &'a [u8],
    pub property_patch: Option<&'a [u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoalescedError {
    NotAContainer,
    UnsupportedFormat { major: u16, minor: u16 },
    Truncated { at: usize },
    TrailingBytes { extra: usize },
    LengthMismatch { declared: u64, found: usize },
    TooLarge { bytes: usize, limit: usize },
    InvalidMemberCount { count: usize },
    LengthOverflow,
    EmptyBlock { at: usize },
    EmptyPatch { at: usize },
    MissingPatch { at: usize },
    UnexpectedPatch { at: usize },
    PartitionMismatch { at: usize, expected: u64, found: u64 },
    Block { at: usize, error: BlockError },
    Patch { at: usize, error: EdgePropertyPatchError },
}

impl core::fmt::Display for CoalescedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAContainer => write!(f, "not a strata seal container"),
            Self::UnsupportedFormat { major, minor } => {
                write!(f, "seal container format {major}.{minor} is not implemented")
            }
            Self::Truncated { at } => write!(f, "seal container ends at framing offset {at}"),
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the last seal member"),
            Self::LengthMismatch { declared, found } => {
                write!(f, "seal container declares {declared} bytes, found {found}")
            }
            Self::TooLarge { bytes, limit } => write!(f, "seal bytes {bytes} exceed limit {limit}"),
            Self::InvalidMemberCount { count } => write!(f, "invalid seal member count {count}"),
            Self::LengthOverflow => write!(f, "seal framing length overflows"),
            Self::EmptyBlock { at } => write!(f, "seal member {at} has no block bytes"),
            Self::EmptyPatch { at } => write!(f, "seal member {at} has an empty present patch"),
            Self::MissingPatch { at } => write!(f, "seal member {at} is missing its hosted patch"),
            Self::UnexpectedPatch { at } => write!(f, "seal member {at} references no patch"),
            Self::PartitionMismatch { at, expected, found } => {
                write!(f, "seal member {at} belongs to partition {found}, not {expected}")
            }
            Self::Block { at, error } => write!(f, "seal member {at}: {error}"),
            Self::Patch { at, error } => write!(f, "seal member {at} patch: {error}"),
        }
    }
}

impl core::error::Error for CoalescedError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Block { error, .. } => Some(error),
            Self::Patch { error, .. } => Some(error),
            _ => None,
        }
    }
}

fn bounded_size(bytes: usize, limit: usize) -> Result<(), CoalescedError> {
    if bytes > limit {
        return Err(CoalescedError::TooLarge { bytes, limit });
    }
    Ok(())
}

fn member_size(at: usize, member: CoalescedBlock<'_>) -> Result<usize, CoalescedError> {
    if member.block_bytes.is_empty() {
        return Err(CoalescedError::EmptyBlock { at });
    }
    if member.property_patch.is_some_and(<[u8]>::is_empty) {
        return Err(CoalescedError::EmptyPatch { at });
    }
    let patch_len = member.property_patch.map_or(0, <[u8]>::len);
    bounded_size(member.block_bytes.len(), MAX_COALESCED_MEMBER_BYTES)?;
    bounded_size(patch_len, MAX_COALESCED_MEMBER_BYTES)?;
    COALESCED_ENTRY_FRAME_LEN
        .checked_add(member.block_bytes.len())
        .and_then(|len| len.checked_add(patch_len))
        .ok_or(CoalescedError::LengthOverflow)
}

fn validate_member(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    at: usize,
    member: CoalescedBlock<'_>,
    partition: &mut Option<u64>,
) -> Result<(), CoalescedError> {
    let block_error = |error| CoalescedError::Block { at, error };
    let patch_error = |error| CoalescedError::Patch { at, error };
    let (entries, patch) = decode_block_with_properties(member.block_bytes).map_err(block_error)?;
    let (found, declared) = header_partition_and_digest(member.block_bytes);
    match *partition {
        Some(expected) if expected != found => {
            return Err(CoalescedError::PartitionMismatch { at, expected, found });
        }
        None => *partition = Some(found),
        _ => {}
    }
    match (patch, member.property_patch) {
        (None, None) => {}
        (None, Some(_)) => return Err(CoalescedError::UnexpectedPatch { at }),
        (Some(_), None) => return Err(CoalescedError::MissingPatch { at }),
        (Some((id, locators)), Some(bytes)) => {
            let rows = read_property_patch(k_oid, namespace, bytes, EdgePropertyPatchVersion(id))
                .map_err(patch_error)?;
            validate_block_patch_consistency(&locators, rows.len()).map_err(patch_error)?;
            // The bijection proves that successive nonzero locators consume
            // successive rows. Move them into entry order; do not clone scalars.
            let mut rows = rows.into_iter();
            let rows_by_entry: Vec<_> = locators
                .into_iter()
                .map(|locator| {
                    if locator == 0 {
                        Vec::new()
                    } else {
                        rows.next().expect("validated locator bijection")
                    }
                })
                .collect();
            let recomputed = block_logical_digest(&entries, &rows_by_entry).map_err(block_error)?;
            if recomputed != declared {
                return Err(block_error(BlockError::LogicalDigestMismatch { declared, recomputed }));
            }
        }
    }
    Ok(())
}

/// Encode in caller-supplied publication order, validating every block and its
/// hosted patch before allocating the output. Same-family chunks are allowed;
/// their cross-object predecessor/commit laws remain admission's responsibility.
pub fn encode_coalesced(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    blocks: &[CoalescedBlock<'_>],
) -> Result<Vec<u8>, CoalescedError> {
    if blocks.is_empty() || blocks.len() > MAX_COALESCED_MEMBERS {
        return Err(CoalescedError::InvalidMemberCount { count: blocks.len() });
    }
    let count = u32::try_from(blocks.len()).map_err(|_| CoalescedError::LengthOverflow)?;
    let mut total = COALESCED_HEADER_LEN;
    for (at, &member) in blocks.iter().enumerate() {
        total = total.checked_add(member_size(at, member)?).ok_or(CoalescedError::LengthOverflow)?;
        bounded_size(total, MAX_COALESCED_BYTES)?;
    }
    let mut partition = None;
    for (at, &member) in blocks.iter().enumerate() {
        validate_member(k_oid, namespace, at, member, &mut partition)?;
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&COALESCED_MAGIC);
    out.extend_from_slice(&COALESCED_FORMAT_V1.to_le_bytes());
    out.extend_from_slice(&COALESCED_FORMAT_MINOR.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(total as u64).to_le_bytes());
    for member in blocks {
        let block_len = u32::try_from(member.block_bytes.len()).map_err(|_| CoalescedError::LengthOverflow)?;
        let patch = member.property_patch.unwrap_or_default();
        let patch_len = u32::try_from(patch.len()).map_err(|_| CoalescedError::LengthOverflow)?;
        out.extend_from_slice(&block_len.to_le_bytes());
        out.extend_from_slice(&patch_len.to_le_bytes());
        out.extend_from_slice(member.block_bytes);
        out.extend_from_slice(patch);
    }
    Ok(out)
}

fn take<'a>(bytes: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], CoalescedError> {
    let end = offset.checked_add(len).ok_or(CoalescedError::LengthOverflow)?;
    let slice = bytes.get(*offset..end).ok_or(CoalescedError::Truncated { at: *offset })?;
    *offset = end;
    Ok(slice)
}

/// Decode borrowed member slices. Framing bounds precede allocation and all
/// framing is checked before interpreting members. Returned block and patch
/// bytes are unchanged, but the caller must still authenticate the enclosing
/// publication and perform root/chain admission before exposing its contents.
pub fn decode_coalesced<'a>(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &'a [u8],
) -> Result<Vec<CoalescedBlock<'a>>, CoalescedError> {
    bounded_size(bytes.len(), MAX_COALESCED_BYTES)?;
    let mut offset = 0;
    let header = take(bytes, &mut offset, COALESCED_HEADER_LEN)?;
    if header[..4] != COALESCED_MAGIC {
        return Err(CoalescedError::NotAContainer);
    }
    let major = u16::from_le_bytes(header[4..6].try_into().expect("fixed header"));
    let minor = u16::from_le_bytes(header[6..8].try_into().expect("fixed header"));
    if major != COALESCED_FORMAT_V1 || minor != COALESCED_FORMAT_MINOR {
        return Err(CoalescedError::UnsupportedFormat { major, minor });
    }
    let count = u32::from_le_bytes(header[8..12].try_into().expect("fixed header")) as usize;
    let declared = u64::from_le_bytes(header[12..20].try_into().expect("fixed header"));
    if declared != bytes.len() as u64 {
        return Err(CoalescedError::LengthMismatch { declared, found: bytes.len() });
    }
    if count == 0 || count > MAX_COALESCED_MEMBERS
        || count > (bytes.len() - COALESCED_HEADER_LEN) / (COALESCED_ENTRY_FRAME_LEN + 1)
    {
        return Err(CoalescedError::InvalidMemberCount { count });
    }
    let mut members = Vec::with_capacity(count);
    for at in 0..count {
        let frame = take(bytes, &mut offset, COALESCED_ENTRY_FRAME_LEN)?;
        let block_len = u32::from_le_bytes(frame[..4].try_into().expect("fixed frame")) as usize;
        let patch_len = u32::from_le_bytes(frame[4..].try_into().expect("fixed frame")) as usize;
        bounded_size(block_len, MAX_COALESCED_MEMBER_BYTES)?;
        bounded_size(patch_len, MAX_COALESCED_MEMBER_BYTES)?;
        let block_bytes = take(bytes, &mut offset, block_len)?;
        let patch = take(bytes, &mut offset, patch_len)?;
        let member = CoalescedBlock {
            block_bytes,
            property_patch: (patch_len != 0).then_some(patch),
        };
        member_size(at, member)?;
        members.push(member);
    }
    if offset != bytes.len() {
        return Err(CoalescedError::TrailingBytes { extra: bytes.len() - offset });
    }
    let mut partition = None;
    for (at, &member) in members.iter().enumerate() {
        validate_member(k_oid, namespace, at, member, &mut partition)?;
    }
    Ok(members)
}
